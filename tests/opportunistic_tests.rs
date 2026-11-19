//! Opportunistic links (NET-22): two nodes at their stable target pass
//! each other, open a short link for the layer above to sync over, and
//! cool down after it.
#![cfg(feature = "sim")]

use std::task::{Context, Poll, Waker};
use std::time::Duration;

use ao_network::{
    AppHello, BeaconFlags, Channel, DisconnectReason, LinkId, LocalTag, Network, NetworkConfig,
    NetworkEvent, PeerKey, SimConfig, SimNode, SimTransport, SimWorld,
};

const TICK: Duration = Duration::from_millis(10);
const COOLDOWN: Duration = Duration::from_secs(60);
const DWELL: Duration = Duration::from_secs(60);

struct Node {
    net: Network<SimTransport>,
    hint: u8,
    events: Vec<NetworkEvent>,
}

impl Node {
    fn key(&self) -> PeerKey {
        self.net.transport().peer_key()
    }

    fn set_digest(&mut self, digest: u8) {
        let flags = BeaconFlags {
            accepting: true,
            ..BeaconFlags::default()
        };
        self.net.set_beacon([self.hint; 8], [digest; 4], flags);
    }

    /// This node's up link to `peer`, if any.
    fn link_to(&self, peer: PeerKey) -> Option<LinkId> {
        let info = self.net.links().into_iter().find(|info| info.peer == peer);
        info.map(|info| info.link)
    }

    /// Link-ups and link-downs with `peer` among the events so far.
    fn ups_with(&self, peer: &Node) -> usize {
        let tag = LocalTag::new([peer.hint; 16]);
        self.events
            .iter()
            .filter(|event| {
                matches!(event, NetworkEvent::LinkUp { remote_hello, .. } if remote_hello.tag == tag)
            })
            .count()
    }
}

/// Node 0 (A, hint 1) with three neighbours and node 4 (B, hint 2) with
/// three of its own, each group in range only of itself. A and B dial
/// their neighbours, whose hints sort higher, and so reach the stable
/// target of three.
fn two_groups() -> (SimWorld, Vec<Node>) {
    let world = SimWorld::new(SimConfig::default());
    let hints = [1, 11, 12, 13, 2, 21, 22, 23];
    let mut nodes: Vec<Node> = hints
        .into_iter()
        .map(|hint| {
            let radio = world.add_node(SimNode::default()).unwrap();
            let mut net = Network::new(NetworkConfig::default(), radio);
            net.set_local_hello(
                LocalTag::new([hint; 16]),
                AppHello::new(Vec::new()).unwrap(),
            );
            Node {
                net,
                hint,
                events: Vec::new(),
            }
        })
        .collect();
    for node in &mut nodes {
        node.set_digest(0);
    }
    for group in [0..4, 4..8] {
        for a in group.clone() {
            for b in a + 1..group.end {
                let (na, nb) = (
                    nodes[a].net.transport().node(),
                    nodes[b].net.transport().node(),
                );
                world.set_in_range(na, nb, true);
            }
        }
    }
    run(&world, &mut nodes, Duration::from_secs(3));
    for centre in [0, 4] {
        assert_eq!(nodes[centre].net.links().len(), 3, "the stable target");
    }
    (world, nodes)
}

fn run(world: &SimWorld, nodes: &mut [Node], duration: Duration) {
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

/// A and B come into range of each other: passing nodes.
fn pass(world: &SimWorld, nodes: &mut [Node]) -> LinkId {
    let (na, nb) = (
        nodes[0].net.transport().node(),
        nodes[4].net.transport().node(),
    );
    world.set_in_range(na, nb, true);
    run(world, nodes, Duration::from_secs(3));
    let b = nodes[4].key();
    nodes[0]
        .link_to(b)
        .expect("A opened an opportunistic link to B")
}

fn down_reason(node: &Node, link: LinkId) -> Option<DisconnectReason> {
    node.events.iter().find_map(|event| match event {
        NetworkEvent::LinkDown { link: l, reason } if *l == link => Some(*reason),
        _ => None,
    })
}

/// Done when: two passing nodes connect, a release from the layer above
/// is honoured, and the pair does not reconnect within the cooldown.
#[test]
fn passing_nodes_sync_release_and_cool_down() {
    let (world, mut nodes) = two_groups();
    let link = pass(&world, &mut nodes);
    // A fourth link at A, above its stable target of three.
    assert_eq!(nodes[0].net.links().len(), 4);

    // The layer above syncs over it and releases it.
    let now = world.now();
    nodes[0]
        .net
        .send(now, link, Channel::Bulk, b"digest sync".to_vec())
        .unwrap();
    run(&world, &mut nodes, Duration::from_secs(1));
    let synced = nodes[4].events.iter().any(
        |event| matches!(event, NetworkEvent::Data { payload, .. } if payload == b"digest sync"),
    );
    assert!(synced);
    let b_link = nodes[4].link_to(nodes[0].key()).unwrap();
    let released_at = world.now();
    nodes[0].net.release(released_at, link);
    run(&world, &mut nodes, Duration::from_secs(1));
    assert_eq!(down_reason(&nodes[0], link), Some(DisconnectReason::Closed));
    assert_eq!(down_reason(&nodes[4], b_link), Some(DisconnectReason::Bye));

    // Still in range, still sighting each other every second: no new
    // link for the rest of the cooldown, from either side.
    let (a, b) = (nodes[0].key(), nodes[4].key());
    let until = released_at + COOLDOWN - Duration::from_secs(1);
    while world.now() < until {
        run(&world, &mut nodes, Duration::from_secs(1));
        assert!(
            nodes[0].link_to(b).is_none(),
            "reconnected at {:?}",
            world.now()
        );
        assert!(
            nodes[4].link_to(a).is_none(),
            "reconnected at {:?}",
            world.now()
        );
    }
    assert_eq!(nodes[0].ups_with(&nodes[4]), 1);

    // Once the cooldown has passed, B is an ordinary candidate again.
    run(&world, &mut nodes, Duration::from_secs(5));
    assert!(nodes[0].link_to(b).is_some(), "the cooldown is temporary");
}

/// The side whose hint sorts higher opens the opportunistic link itself,
/// once the initiation delay has passed, when the other side never
/// dials: here B advertises that it is not accepting, so A cannot dial
/// it. The trigger outlives the sighting the delay first held back.
#[test]
fn the_higher_hint_opens_an_opportunistic_link_after_the_delay() {
    let (world, mut nodes) = two_groups();
    nodes[4]
        .net
        .set_beacon([2; 8], [0; 4], BeaconFlags::default());
    let (na, nb) = (
        nodes[0].net.transport().node(),
        nodes[4].net.transport().node(),
    );
    world.set_in_range(na, nb, true);
    let a = nodes[0].key();
    run(&world, &mut nodes, Duration::from_secs(3));
    assert!(nodes[4].link_to(a).is_none(), "held back by the delay");
    run(&world, &mut nodes, Duration::from_secs(3));
    let link = nodes[4].link_to(a).expect("B dialed after the delay");

    // Only an opportunistic link answers release.
    let now = world.now();
    nodes[4].net.release(now, link);
    run(&world, &mut nodes, Duration::from_secs(1));
    assert_eq!(down_reason(&nodes[4], link), Some(DisconnectReason::Closed));
}

/// A new digest lifts the cooldown at once: the peer has something new
/// to sync.
#[test]
fn a_new_digest_lifts_the_cooldown() {
    let (world, mut nodes) = two_groups();
    let link = pass(&world, &mut nodes);
    let now = world.now();
    nodes[0].net.release(now, link);
    run(&world, &mut nodes, Duration::from_secs(5));
    let b = nodes[4].key();
    assert!(nodes[0].link_to(b).is_none(), "cooling down");

    nodes[4].set_digest(7);
    run(&world, &mut nodes, Duration::from_secs(3));
    assert!(
        nodes[0].link_to(b).is_some(),
        "the new digest opened a link"
    );
    assert_eq!(nodes[0].ups_with(&nodes[4]), 2);
}

/// An opportunistic link nobody releases ends after its dwell, as a
/// degree-cap closure, and then cools down like a released one.
#[test]
fn an_unreleased_link_ends_after_its_dwell() {
    let (world, mut nodes) = two_groups();
    let opened_at = world.now();
    let link = pass(&world, &mut nodes);
    let b_link = nodes[4].link_to(nodes[0].key()).unwrap();
    run(
        &world,
        &mut nodes,
        DWELL - (world.now() - opened_at) - Duration::from_secs(1),
    );
    assert!(
        down_reason(&nodes[0], link).is_none(),
        "still within its dwell"
    );
    run(&world, &mut nodes, Duration::from_secs(3));
    assert_eq!(
        down_reason(&nodes[0], link),
        Some(DisconnectReason::DegreeCap)
    );
    assert_eq!(down_reason(&nodes[4], b_link), Some(DisconnectReason::Bye));
    run(&world, &mut nodes, Duration::from_secs(30));
    assert!(nodes[0].link_to(nodes[4].key()).is_none(), "cooling down");
}

/// Release is for opportunistic links only: on a stable link it does
/// nothing, and so does an unknown link.
#[test]
fn release_leaves_other_links_alone() {
    let (world, mut nodes) = two_groups();
    let stable = nodes[0].link_to(nodes[1].key()).unwrap();
    let now = world.now();
    nodes[0].net.release(now, stable);
    nodes[0].net.release(now, LinkId::new(u64::MAX));
    run(&world, &mut nodes, Duration::from_secs(2));
    assert_eq!(nodes[0].net.links().len(), 3);
    assert!(down_reason(&nodes[0], stable).is_none());
}
