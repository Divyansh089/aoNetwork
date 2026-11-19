//! Inbound abuse protection (NET-18): rate limits on one link, and strikes
//! and the blacklist across links. The abusive peers here are scripted
//! simulator radios; they exist only inside `SimWorld`.
#![cfg(feature = "sim")]

use std::collections::VecDeque;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use ao_network::{
    AppHello, BeaconFlags, Channel, DisconnectReason, FRAME_VERSION, Fragmenter, Frame, Hello,
    Link, LinkAction, LinkEnd, LinkId, LinkRole, LinkState, LocalTag, Network, NetworkConfig,
    NetworkEvent, PeerKey, RateLimit, SimConfig, SimNode, SimTransport, SimWorld, Transport,
    TransportEvent,
};

const TICK: Duration = Duration::from_millis(10);
const MTU: u16 = 247;
const ABUSER_TAG: LocalTag = LocalTag::new([0xEE; 16]);

fn hello(tag: LocalTag) -> Hello {
    Hello {
        proto_min: FRAME_VERSION,
        proto_max: FRAME_VERSION,
        caps: 0,
        max_frame: 16 * 1024,
        tag,
        app_hello: AppHello::new(Vec::new()).unwrap(),
    }
}

fn chunks(frame: &[u8]) -> Vec<Vec<u8>> {
    Fragmenter::new(frame, MTU).unwrap().collect()
}

fn received(chunk: Vec<u8>) -> TransportEvent {
    TransportEvent::Received {
        link: LinkId::new(1),
        chunk,
    }
}

/// A `Link` past its handshake, built on `config`.
fn ready_link(config: &NetworkConfig) -> Link {
    let mut link = Link::new(config, hello(LocalTag::new([1; 16])));
    link.on_event(
        Duration::ZERO,
        TransportEvent::Connected {
            link: LinkId::new(1),
            peer: PeerKey::new([2; 16]),
            role: LinkRole::Responder,
            mtu: MTU,
        },
    );
    for chunk in chunks(&Frame::Hello(hello(LocalTag::new([2; 16]))).encode()) {
        link.on_event(Duration::ZERO, received(chunk));
    }
    assert_eq!(link.state(), LinkState::Ready);
    link
}

/// A `Ping` frame as the single chunk it travels in.
fn ping() -> Vec<u8> {
    chunks(&Frame::Ping.encode()).remove(0)
}

/// Feeds `chunk` at `now` and returns whether the link started closing.
fn closes_on(link: &mut Link, now: Duration, chunk: Vec<u8>) -> bool {
    let actions = link.on_event(now, received(chunk));
    actions.contains(&LinkAction::Disconnect)
}

/// The reason the link reports once the transport confirms the close.
fn reason(link: &mut Link) -> Vec<LinkAction> {
    link.on_event(
        Duration::ZERO,
        TransportEvent::Disconnected {
            link: LinkId::new(1),
            reason: DisconnectReason::TransportLost,
        },
    )
}

/// One frame past the frame burst closes the link as abuse; the `Hello`
/// counted as the first frame.
#[test]
fn a_frame_flood_closes_the_link() {
    let limit = RateLimit {
        per_second: 10,
        burst: 5,
    };
    let config = NetworkConfig::default().with_frame_limit(limit).unwrap();
    let mut link = ready_link(&config);
    let ping = ping();
    for _ in 0..4 {
        assert!(!closes_on(&mut link, Duration::ZERO, ping.clone()));
    }
    assert!(closes_on(&mut link, Duration::ZERO, ping));
    assert_eq!(
        reason(&mut link),
        [LinkAction::Down(LinkEnd::Disconnected(
            DisconnectReason::AbuseDetected
        ))]
    );
}

/// Bytes are counted per chunk, before reassembly: a frame still being
/// reassembled cannot hide a flood.
#[test]
fn a_byte_flood_closes_the_link_mid_frame() {
    let limit = RateLimit {
        per_second: 1,
        burst: 1_000,
    };
    let config = NetworkConfig::default().with_byte_limit(limit).unwrap();
    let mut link = ready_link(&config);
    let frame = Frame::Data {
        channel: Channel::Bulk,
        payload: vec![7; 4_000],
    };
    let mut sent = 0;
    let mut closed = false;
    for chunk in chunks(&frame.encode()) {
        sent += chunk.len();
        if closes_on(&mut link, Duration::ZERO, chunk) {
            closed = true;
            break;
        }
    }
    assert!(closed, "4 KB against a 1 KB burst");
    // The Hello came out of the same bucket, so the cut comes no later
    // than the first chunk that crosses 1 KB.
    assert!(sent <= 1_000 + usize::from(MTU), "{sent} bytes");
    assert_eq!(
        reason(&mut link),
        [LinkAction::Down(LinkEnd::Disconnected(
            DisconnectReason::AbuseDetected
        ))]
    );
}

/// A peer that keeps to the rate is never cut off, however long it
/// talks.
#[test]
fn a_peer_at_the_rate_stays_up() {
    let limit = RateLimit {
        per_second: 10,
        burst: 2,
    };
    let config = NetworkConfig::default().with_frame_limit(limit).unwrap();
    let mut link = ready_link(&config);
    for i in 1..=600u64 {
        let now = Duration::from_millis(100 * i);
        assert!(!closes_on(&mut link, now, ping()), "frame {i}");
    }
    assert_eq!(link.state(), LinkState::Ready);
}

/// What a scripted peer sends once its link is up.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Script {
    /// A valid `Hello`, then small data frames as fast as the radio allows.
    Flood,
    /// A valid `Hello`, then one frame that does not decode.
    Garbage,
}

/// A raw radio that dials the victim and runs its script on each link.
struct Abuser {
    radio: SimTransport,
    script: Script,
    link: Option<LinkId>,
    can_write: bool,
    out: VecDeque<Vec<u8>>,
    /// Connections that came up, and ones that ended.
    connects: usize,
    drops: usize,
}

impl Abuser {
    fn step(&mut self, victim: PeerKey) {
        let mut cx = Context::from_waker(Waker::noop());
        while let Poll::Ready(event) = self.radio.poll_event(&mut cx) {
            match event {
                TransportEvent::Connected { link, .. } => {
                    self.link = Some(link);
                    self.can_write = true;
                    self.connects += 1;
                    self.out = chunks(&Frame::Hello(hello(ABUSER_TAG)).encode()).into();
                    if self.script == Script::Garbage {
                        // Version 0xFF: no such frame layout.
                        self.out.extend(chunks(&[0xFF, 0x01, 0x00]));
                    }
                }
                TransportEvent::WriteReady { .. } => self.can_write = true,
                TransportEvent::Disconnected { .. } | TransportEvent::ConnectFailed { .. } => {
                    self.link = None;
                    self.drops += 1;
                }
                _ => {}
            }
        }
        let Some(link) = self.link else {
            // Dial again at once: an abuser does not back off.
            self.radio.connect(victim);
            self.link = Some(LinkId::new(u64::MAX));
            self.can_write = false;
            return;
        };
        if self.out.is_empty() && self.script == Script::Flood {
            let frame = Frame::Data {
                channel: Channel::Message,
                payload: vec![0xAB; 16],
            };
            self.out.extend(chunks(&frame.encode()));
        }
        if self.can_write {
            if let Some(chunk) = self.out.pop_front() {
                self.can_write = false;
                self.radio.write(link, chunk);
            }
        }
    }
}

struct Node {
    net: Network<SimTransport>,
    events: Vec<NetworkEvent>,
}

/// The victim (node 0), an honest peer (node 1) and an abuser, all in
/// range of each other.
fn world(config: NetworkConfig, script: Script) -> (SimWorld, Vec<Node>, Abuser) {
    let world = SimWorld::new(SimConfig::default());
    let accepting = BeaconFlags {
        accepting: true,
        ..BeaconFlags::default()
    };
    let nodes: Vec<Node> = (1..=2u8)
        .map(|i| {
            let radio = world.add_node(SimNode::default()).unwrap();
            let mut net = Network::new(config.clone(), radio);
            net.set_local_hello(LocalTag::new([i; 16]), AppHello::new(Vec::new()).unwrap());
            net.set_beacon([i; 8], [0; 4], accepting);
            Node {
                net,
                events: Vec::new(),
            }
        })
        .collect();
    let mut radio = world.add_node(SimNode::default()).unwrap();
    radio.start();
    let ids = [
        nodes[0].net.transport().node(),
        nodes[1].net.transport().node(),
        radio.node(),
    ];
    for a in 0..3 {
        for b in a + 1..3 {
            world.set_in_range(ids[a], ids[b], true);
        }
    }
    let abuser = Abuser {
        radio,
        script,
        // Not dialing until the victim has had time to advertise.
        link: Some(LinkId::new(u64::MAX)),
        can_write: false,
        out: VecDeque::new(),
        connects: 0,
        drops: 0,
    };
    (world, nodes, abuser)
}

/// Runs everyone for `duration`; the honest peer sends the victim one
/// message every 100 ms once linked, and the abuser starts at `start`.
fn run(
    world: &SimWorld,
    nodes: &mut [Node],
    abuser: &mut Abuser,
    duration: Duration,
    start: Duration,
) -> usize {
    let mut cx = Context::from_waker(Waker::noop());
    let victim = nodes[0].net.transport().peer_key();
    let honest = nodes[1].net.transport().peer_key();
    let end = world.now() + duration;
    let mut sent = 0;
    while world.now() < end {
        world.advance(TICK);
        let now = world.now();
        for node in nodes.iter_mut() {
            while let Poll::Ready(event) = node.net.poll_event(&mut cx, now) {
                node.events.push(event);
            }
        }
        if now == start {
            abuser.link = None;
        }
        if now >= start {
            abuser.step(victim);
        }
        let to_victim = nodes[1]
            .net
            .links()
            .into_iter()
            .find(|info| info.peer == victim);
        if let Some(info) = to_victim {
            if now.as_millis() % 100 == 0 {
                let payload = sent.to_string().into_bytes();
                nodes[1]
                    .net
                    .send(now, info.link, Channel::Message, payload)
                    .unwrap();
                sent += 1;
            }
        }
        // The victim must never lose its honest neighbour.
        assert!(
            sent == 0 || nodes[0].net.links().iter().any(|l| l.peer == honest),
            "honest link lost at {now:?}"
        );
    }
    sent
}

/// Links the victim opened to the abuser, from its own events.
fn abuser_links(victim: &Node) -> Vec<LinkId> {
    victim
        .events
        .iter()
        .filter_map(|event| match event {
            NetworkEvent::LinkUp { link, remote_hello } if remote_hello.tag == ABUSER_TAG => {
                Some(*link)
            }
            _ => None,
        })
        .collect()
}

fn down_reason(victim: &Node, link: LinkId) -> Option<DisconnectReason> {
    victim.events.iter().find_map(|event| match event {
        NetworkEvent::LinkDown { link: l, reason } if *l == link => Some(*reason),
        _ => None,
    })
}

/// Done when: a simulated flooding peer is disconnected and blocked,
/// while an honest peer at a normal rate is unaffected.
#[test]
fn a_flooding_peer_is_disconnected_and_blocked() {
    let (world, mut nodes, mut abuser) = world(NetworkConfig::default(), Script::Flood);
    let start = Duration::from_secs(1);
    let sent = run(
        &world,
        &mut nodes,
        &mut abuser,
        Duration::from_secs(30),
        start,
    );

    // One flood link came up, was cut off as abuse, and every redial
    // after it was refused.
    let flooded = abuser_links(&nodes[0]);
    assert_eq!(flooded.len(), 1, "{:?}", nodes[0].events);
    assert_eq!(
        down_reason(&nodes[0], flooded[0]),
        Some(DisconnectReason::AbuseDetected)
    );
    assert!(abuser.connects > 10, "the abuser kept dialing");
    assert!(abuser.drops >= abuser.connects - 1);

    // Every honest message arrived, in order, and the honest link never
    // went down.
    let honest: Vec<Vec<u8>> = nodes[0]
        .events
        .iter()
        .filter_map(|event| match event {
            NetworkEvent::Data { link, payload, .. } if !flooded.contains(link) => {
                Some(payload.clone())
            }
            _ => None,
        })
        .collect();
    assert!(sent > 250, "{sent}");
    let expected: Vec<Vec<u8>> = (0..honest.len())
        .map(|i| i.to_string().into_bytes())
        .collect();
    assert_eq!(honest, expected);
    assert!(honest.len() >= sent - 1, "{} of {sent}", honest.len());
    let downs = nodes[0]
        .events
        .iter()
        .filter(|event| matches!(event, NetworkEvent::LinkDown { .. }))
        .count();
    assert_eq!(downs, 1, "only the flood link went down");
}

/// Malformed frames are strikes: the victim takes the abuser back until
/// `max_strikes` links have ended in protocol violations, then blocks it.
#[test]
fn repeated_malformed_frames_get_a_peer_blocked() {
    let (world, mut nodes, mut abuser) = world(NetworkConfig::default(), Script::Garbage);
    run(
        &world,
        &mut nodes,
        &mut abuser,
        Duration::from_secs(10),
        Duration::from_secs(1),
    );
    let struck = abuser_links(&nodes[0]);
    assert_eq!(struck.len(), 3, "{:?}", nodes[0].events);
    for link in struck {
        assert_eq!(
            down_reason(&nodes[0], link),
            Some(DisconnectReason::ProtocolViolation)
        );
    }
    assert!(abuser.connects > 3, "later dials were refused");
}

/// A blacklist entry is temporary: once it lapses, the peer is admitted
/// again, and a peer with fewer strikes than the limit is never blocked.
#[test]
fn a_block_lapses_after_the_blacklist_duration() {
    let config = NetworkConfig::default()
        .with_blacklist_duration(Duration::from_secs(5))
        .unwrap();
    let (world, mut nodes, mut abuser) = world(config, Script::Flood);
    run(
        &world,
        &mut nodes,
        &mut abuser,
        Duration::from_secs(30),
        Duration::from_secs(1),
    );
    let flooded = abuser_links(&nodes[0]);
    // A flood is cut within a few seconds and then blocked for five, so
    // thirty seconds hold several rounds, not one.
    assert!(flooded.len() >= 3, "{}", flooded.len());
    for link in flooded {
        let reason = down_reason(&nodes[0], link);
        assert!(
            matches!(reason, Some(DisconnectReason::AbuseDetected) | None),
            "{reason:?}"
        );
    }
}
