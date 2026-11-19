//! Duplicate-link resolution (NET-17): a pair of nodes ends with one link
//! however their connection attempts race.
#![cfg(feature = "sim")]

use std::task::{Context, Poll, Waker};
use std::time::Duration;

use ao_network::{
    AdvProfile, AppHello, BeaconFlags, DisconnectReason, LinkId, LinkRole, LocalTag, Network,
    NetworkConfig, NetworkEvent, PeerKey, ScanProfile, SimConfig, SimNode, SimTransport, SimWorld,
    Transport, TransportCaps, TransportEvent,
};

const TICK: Duration = Duration::from_millis(10);
/// Long enough for the delayed dial, both handshakes and any tie-break.
const SETTLE: Duration = Duration::from_secs(12);

/// A simulator radio that counts the connections its node dials.
struct Counted {
    radio: SimTransport,
    dials: usize,
}

impl Transport for Counted {
    fn caps(&self) -> TransportCaps {
        self.radio.caps()
    }
    fn start(&mut self) {
        self.radio.start();
    }
    fn stop(&mut self) {
        self.radio.stop();
    }
    fn set_advertising(&mut self, profile: Option<AdvProfile>) {
        self.radio.set_advertising(profile);
    }
    fn set_scanning(&mut self, profile: Option<ScanProfile>) {
        self.radio.set_scanning(profile);
    }
    fn connect(&mut self, peer: PeerKey) -> LinkId {
        self.dials += 1;
        self.radio.connect(peer)
    }
    fn disconnect(&mut self, link: LinkId) {
        self.radio.disconnect(link);
    }
    fn write(&mut self, link: LinkId, chunk: Vec<u8>) {
        self.radio.write(link, chunk);
    }
    fn poll_event(&mut self, cx: &mut Context<'_>) -> Poll<TransportEvent> {
        self.radio.poll_event(cx)
    }
}

struct Node {
    net: Network<Counted>,
    tag: LocalTag,
    events: Vec<NetworkEvent>,
}

/// Two nodes, out of range, with the given tags and hints.
fn pair(config: SimConfig, tags: [u8; 2], hints: [u8; 2]) -> (SimWorld, Vec<Node>) {
    let world = SimWorld::new(config);
    let accepting = BeaconFlags {
        accepting: true,
        ..BeaconFlags::default()
    };
    let nodes = tags
        .into_iter()
        .zip(hints)
        .map(|(tag, hint)| {
            let radio = world.add_node(SimNode::default()).unwrap();
            let mut net = Network::new(NetworkConfig::default(), Counted { radio, dials: 0 });
            let tag = LocalTag::new([tag; 16]);
            net.set_local_hello(tag, AppHello::new(Vec::new()).unwrap());
            net.set_beacon([hint; 8], [0; 4], accepting);
            Node {
                net,
                tag,
                events: Vec::new(),
            }
        })
        .collect();
    (world, nodes)
}

/// Brings the two nodes into range of each other at the same instant and
/// runs them for `duration`.
fn meet(world: &SimWorld, nodes: &mut [Node], duration: Duration) {
    let (a, b) = (
        nodes[0].net.transport().radio.node(),
        nodes[1].net.transport().radio.node(),
    );
    world.set_in_range(a, b, true);
    let mut cx = Context::from_waker(Waker::noop());
    let end = world.now() + duration;
    while world.now() < end {
        world.advance(TICK);
        for node in nodes.iter_mut() {
            while let Poll::Ready(event) = node.net.poll_event(&mut cx, world.now()) {
                node.events.push(event);
            }
        }
    }
}

/// Up links announced minus up links ended.
fn net_ups(node: &Node) -> isize {
    node.events
        .iter()
        .map(|event| match event {
            NetworkEvent::LinkUp { .. } => 1,
            NetworkEvent::LinkDown { .. } => -1,
            _ => 0,
        })
        .sum()
}

/// A small deterministic generator for per-run tags and hints.
fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Asserts the pair holds exactly one link, opened by `opener` and so
/// accepted by the other node, with each event stream ending one link up.
fn assert_one_link(nodes: &[Node], opener: usize, context: &str) {
    for (i, node) in nodes.iter().enumerate() {
        let links = node.net.links();
        assert_eq!(links.len(), 1, "{context}");
        assert_eq!(net_ups(node), 1, "{context}: {:?}", node.events);
        let role = if i == opener {
            LinkRole::Initiator
        } else {
            LinkRole::Responder
        };
        assert_eq!(links[0].role, role, "{context}");
    }
}

/// Done when: two nodes that discover each other at the same instant end
/// with exactly one link, in 1000 of 1000 seeded runs. In about a quarter
/// of the runs both nodes have the same hint and dial at once; the second
/// connection is refused if the first is up by then, and otherwise the tag
/// tie-break closes one once both are. In the rest the lower hint dials
/// first and the delay keeps the other side from dialing at all.
#[test]
fn simultaneous_discovery_ends_with_one_link_in_every_seeded_run() {
    let mut races = 0;
    for seed in 0..1000u64 {
        let mut rng = seed;
        let tag_a = (splitmix(&mut rng) % 255) as u8;
        let tag_b = tag_a + 1 + (splitmix(&mut rng) % u64::from(255 - tag_a)) as u8;
        let hints = [
            (splitmix(&mut rng) % 4) as u8,
            (splitmix(&mut rng) % 4) as u8,
        ];
        let tags = if splitmix(&mut rng) % 2 == 0 {
            [tag_a, tag_b]
        } else {
            [tag_b, tag_a]
        };
        let config = SimConfig {
            seed,
            ..SimConfig::default()
        };
        let (world, mut nodes) = pair(config, tags, hints);
        meet(&world, &mut nodes, SETTLE);

        let context = format!("seed {seed}, tags {tags:?}, hints {hints:?}");
        let dials: Vec<usize> = nodes.iter().map(|n| n.net.transport().dials).collect();
        assert!(nodes.iter().all(|n| n.net.links().len() == 1), "{context}");
        let opener = usize::from(nodes[1].net.links()[0].role == LinkRole::Initiator);
        assert_one_link(&nodes, opener, &context);
        if hints[0] == hints[1] {
            races += usize::from(dials == [1, 1]);
        } else {
            assert_eq!(dials.iter().sum::<usize>(), 1, "{context}: {dials:?}");
            let low_hint = usize::from(hints[1] < hints[0]);
            assert_eq!(opener, low_hint, "{context}");
        }
    }
    assert!(races > 100, "both sides dialed in only {races} runs");
}

/// Both handshakes finish together: each end sees two ready links to the
/// same tag, and both close the one opened by the higher tag, so the
/// survivor is the same connection at both ends. An end that announced
/// the losing link before the winner was ready reports it down as
/// `Duplicate`; a loser not yet announced ends silently.
#[test]
fn a_simultaneous_connect_keeps_the_link_opened_by_the_lower_tag() {
    let mut swapped = 0;
    for tags in [[1, 2], [2, 1]] {
        let config = SimConfig {
            connect_jitter: Duration::ZERO,
            ..SimConfig::default()
        };
        let (world, mut nodes) = pair(config, tags, [7, 7]);
        meet(&world, &mut nodes, SETTLE);

        let context = format!("tags {tags:?}");
        let dials: Vec<usize> = nodes.iter().map(|n| n.net.transport().dials).collect();
        assert_eq!(dials, [1, 1], "{context}");
        let low_tag = usize::from(nodes[1].tag < nodes[0].tag);
        assert_one_link(&nodes, low_tag, &context);
        for node in &nodes {
            for event in &node.events {
                match event {
                    NetworkEvent::LinkUp { .. } => {}
                    NetworkEvent::LinkDown { reason, .. } => {
                        assert_eq!(*reason, DisconnectReason::Duplicate, "{context}");
                        swapped += 1;
                    }
                    other => panic!("{context}: unexpected {other:?}"),
                }
            }
        }
    }
    assert!(swapped > 0, "an announced link never lost the tie-break");
}

/// A peer that introduces itself with this node's own tag is dropped at
/// the handshake, never announced, and redialed only after a backoff.
#[test]
fn a_peer_with_our_own_tag_is_dropped() {
    let (world, mut nodes) = pair(SimConfig::default(), [5, 5], [1, 2]);
    meet(&world, &mut nodes, Duration::from_secs(10));
    for node in &nodes {
        assert!(node.net.links().is_empty());
        assert!(node.events.is_empty(), "{:?}", node.events);
    }
    // Each failure doubles the backoff (1 s, 2 s, 4 s, plus jitter), so
    // ten seconds hold a handful of attempts, not one per sighting.
    let dials: usize = nodes.iter().map(|n| n.net.transport().dials).sum();
    assert!((1..=6).contains(&dials), "{dials} dials");
}

/// The higher-hint side dials once the delay has passed when the other
/// side never does: here the higher-hint node advertises that it is not
/// accepting, so only it can open the link. Before NET-17 such a pair
/// never linked.
#[test]
fn the_higher_hint_dials_after_the_delay_when_the_other_side_does_not() {
    let (world, mut nodes) = pair(SimConfig::default(), [1, 2], [1, 2]);
    let closed = BeaconFlags {
        accepting: false,
        ..BeaconFlags::default()
    };
    nodes[1].net.set_beacon([2; 8], [0; 4], closed);

    meet(&world, &mut nodes, Duration::from_millis(3_900));
    assert!(
        nodes.iter().all(|n| n.net.links().is_empty()),
        "dialed early"
    );
    meet(&world, &mut nodes, SETTLE);
    for node in &nodes {
        assert_eq!(node.net.links().len(), 1);
        assert_eq!(net_ups(node), 1, "{:?}", node.events);
    }
    assert_eq!(nodes[1].net.links()[0].role, LinkRole::Initiator);
    let dials: Vec<usize> = nodes.iter().map(|n| n.net.transport().dials).collect();
    assert_eq!(dials, [0, 1]);
}
