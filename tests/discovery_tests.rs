//! Discovery modes (NET-19) end to end: what the network asks of its
//! radio as its degree, battery and idle state change.
#![cfg(feature = "sim")]

use std::task::{Context, Poll, Waker};
use std::time::Duration;

use ao_network::{
    AdvProfile, AppHello, BeaconFlags, DiscoveryMode, LinkId, LocalTag, Network, NetworkConfig,
    PeerKey, PowerMode, ScanProfile, SimConfig, SimNode, SimTransport, SimWorld, Transport,
    TransportCaps, TransportEvent,
};

const TICK: Duration = Duration::from_millis(10);
const INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Request {
    Scan(Option<ScanProfile>),
    Advertise(Option<AdvProfile>),
}

/// A simulator radio that logs every profile request with its time.
struct Watched {
    radio: SimTransport,
    world: SimWorld,
    log: Vec<(Duration, Request)>,
}

impl Transport for Watched {
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
        self.log
            .push((self.world.now(), Request::Advertise(profile)));
        self.radio.set_advertising(profile);
    }
    fn set_scanning(&mut self, profile: Option<ScanProfile>) {
        self.log.push((self.world.now(), Request::Scan(profile)));
        self.radio.set_scanning(profile);
    }
    fn connect(&mut self, peer: PeerKey) -> LinkId {
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

fn flags(low_battery: bool) -> BeaconFlags {
    BeaconFlags {
        accepting: true,
        low_battery,
        ..BeaconFlags::default()
    }
}

/// The watched node 0 and five plain peers, none in range yet. Node 0's
/// hint sorts lowest, so it dials every peer it sees.
fn world() -> (SimWorld, Network<Watched>, Vec<Network<SimTransport>>) {
    let world = SimWorld::new(SimConfig::default());
    let radio = world.add_node(SimNode::default()).unwrap();
    let watched = Watched {
        radio,
        world: world.clone(),
        log: Vec::new(),
    };
    let mut node = Network::new(NetworkConfig::default(), watched);
    node.set_local_hello(LocalTag::new([0; 16]), AppHello::new(Vec::new()).unwrap());
    node.set_beacon([0; 8], [0; 4], flags(false));
    let peers = (1..=5u8)
        .map(|i| {
            let radio = world.add_node(SimNode::default()).unwrap();
            let mut net = Network::new(NetworkConfig::default(), radio);
            net.set_local_hello(LocalTag::new([i; 16]), AppHello::new(Vec::new()).unwrap());
            net.set_beacon([i; 8], [0; 4], flags(false));
            net
        })
        .collect();
    (world, node, peers)
}

fn run(
    world: &SimWorld,
    node: &mut Network<Watched>,
    peers: &mut [Network<SimTransport>],
    duration: Duration,
) {
    let mut cx = Context::from_waker(Waker::noop());
    let end = world.now() + duration;
    while world.now() < end {
        world.advance(TICK);
        while node.poll_event(&mut cx, world.now()).is_ready() {}
        for peer in peers.iter_mut() {
            while peer.poll_event(&mut cx, world.now()).is_ready() {}
        }
    }
}

fn in_range(world: &SimWorld, node: &Network<Watched>, peer: &Network<SimTransport>, on: bool) {
    world.set_in_range(node.transport().radio.node(), peer.transport().node(), on);
}

/// The scan and advertise modes last requested.
fn last_modes(node: &Network<Watched>) -> (Option<PowerMode>, Option<PowerMode>) {
    let log = &node.transport().log;
    let scan = log.iter().rev().find_map(|(_, request)| match request {
        Request::Scan(profile) => Some(profile.map(|p| p.mode)),
        Request::Advertise(_) => None,
    });
    let advertise = log.iter().rev().find_map(|(_, request)| match request {
        Request::Advertise(profile) => Some(profile.map(|p| p.mode)),
        Request::Scan(_) => None,
    });
    (scan.flatten(), advertise.flatten())
}

/// The requests that start, stop or change the mode of the scanner or
/// the advertiser, as opposed to a new beacon in the same mode.
fn mode_changes(log: &[(Duration, Request)]) -> Vec<(Duration, Request)> {
    let mut scan_mode = None;
    let mut adv_mode = None;
    let mut out = Vec::new();
    for &(at, request) in log {
        let changed = match request {
            Request::Scan(profile) => {
                let mode = profile.map(|p| p.mode);
                scan_mode.replace(mode) != Some(mode)
            }
            Request::Advertise(profile) => {
                let mode = profile.map(|p| p.mode);
                adv_mode.replace(mode) != Some(mode)
            }
        };
        if changed {
            out.push((at, request));
        }
    }
    out
}

/// The mode follows the degree and battery inputs through the network,
/// and churn, a flapping battery flag and a flapping idle state never get
/// a mode change requested sooner than the interval after the last.
#[test]
fn modes_follow_the_inputs_and_changes_respect_the_interval() {
    let (world, mut node, mut peers) = world();
    run(&world, &mut node, &mut peers, Duration::from_secs(1));
    assert_eq!(node.discovery_mode(), DiscoveryMode::Aggressive);
    assert_eq!(
        last_modes(&node),
        (Some(PowerMode::LowLatency), Some(PowerMode::LowLatency))
    );

    // Three neighbours reach the stable target: normal.
    for peer in &peers[..3] {
        in_range(&world, &node, peer, true);
    }
    run(&world, &mut node, &mut peers, Duration::from_secs(15));
    assert_eq!(node.links().len(), 3);
    assert_eq!(node.discovery_mode(), DiscoveryMode::Normal);
    assert_eq!(
        last_modes(&node),
        (Some(PowerMode::Balanced), Some(PowerMode::Balanced))
    );

    // The battery runs low: quiet.
    node.set_beacon([0; 8], [0; 4], flags(true));
    run(&world, &mut node, &mut peers, Duration::from_secs(15));
    assert_eq!(node.discovery_mode(), DiscoveryMode::Quiet);

    // Charged, then a neighbour leaves: below target, aggressive.
    node.set_beacon([0; 8], [0; 4], flags(false));
    in_range(&world, &node, &peers[0], false);
    run(&world, &mut node, &mut peers, Duration::from_secs(15));
    assert_eq!(node.links().len(), 2);
    assert_eq!(node.discovery_mode(), DiscoveryMode::Aggressive);

    // Churn and flapping inputs, several times faster than the interval.
    let mut cx = Context::from_waker(Waker::noop());
    for step in 0..2_000u32 {
        if step % 37 == 0 {
            node.set_idle(step % 74 == 0);
        }
        if step % 53 == 0 {
            node.set_beacon([0; 8], [0; 4], flags(step % 106 == 0));
        }
        if step % 91 == 0 {
            let peer = 1 + (step as usize / 91) % 4;
            in_range(&world, &node, &peers[peer], step % 182 == 0);
        }
        world.advance(Duration::from_millis(100));
        while node.poll_event(&mut cx, world.now()).is_ready() {}
        for peer in peers.iter_mut() {
            while peer.poll_event(&mut cx, world.now()).is_ready() {}
        }
    }

    let changes = mode_changes(&node.transport().log);
    assert!(changes.len() > 10, "{} changes", changes.len());
    // Requests made in one poll go out together; separate polls are an
    // interval apart at least.
    let mut times: Vec<Duration> = changes.iter().map(|(at, _)| *at).collect();
    times.dedup();
    for pair in times.windows(2) {
        assert!(pair[1] - pair[0] >= INTERVAL, "{pair:?}");
    }
}

/// Quiet scanning runs in windows of one interval with pauses of five,
/// while the beacon keeps advertising; a new beacon in the same mode is
/// not held back by the interval.
#[test]
fn quiet_scans_in_windows_and_beacons_are_not_held_back() {
    let (world, mut node, mut peers) = world();
    run(&world, &mut node, &mut peers, Duration::from_secs(1));
    node.set_idle(true);
    run(&world, &mut node, &mut peers, Duration::from_secs(150));
    assert_eq!(node.discovery_mode(), DiscoveryMode::Quiet);

    let start = node.transport().log.len();
    run(&world, &mut node, &mut peers, Duration::from_secs(120));
    let scans: Vec<(Duration, Option<ScanProfile>)> = node.transport().log[start..]
        .iter()
        .filter_map(|&(at, request)| match request {
            Request::Scan(profile) => Some((at, profile)),
            Request::Advertise(_) => None,
        })
        .collect();
    assert!(scans.len() >= 3, "{scans:?}");
    for pair in scans.windows(2) {
        let gap = pair[1].0 - pair[0].0;
        let expected = if pair[0].1.is_some() {
            INTERVAL
        } else {
            INTERVAL * 5
        };
        assert_eq!(gap, expected, "{scans:?}");
        assert_eq!(pair[0].1.is_some(), pair[1].1.is_none(), "{scans:?}");
    }

    // A neighbour arrives: the degree in the beacon changes at once, in
    // the quiet mode, whatever the interval says.
    let before = node.transport().log.len();
    in_range(&world, &node, &peers[0], true);
    let mut cx = Context::from_waker(Waker::noop());
    let mut linked_at = None;
    for _ in 0..12_000 {
        world.advance(TICK);
        while node.poll_event(&mut cx, world.now()).is_ready() {}
        for peer in peers.iter_mut() {
            while peer.poll_event(&mut cx, world.now()).is_ready() {}
        }
        if !node.links().is_empty() {
            linked_at = Some(world.now());
            break;
        }
    }
    let linked_at = linked_at.expect("the neighbour linked during a scan window");
    let adverts: Vec<(Duration, AdvProfile)> = node.transport().log[before..]
        .iter()
        .filter_map(|&(at, request)| match request {
            Request::Advertise(Some(profile)) => Some((at, profile)),
            _ => None,
        })
        .collect();
    assert!(
        adverts.iter().all(|(_, p)| p.mode == PowerMode::LowPower),
        "{adverts:?}"
    );
    assert!(
        adverts.iter().any(|(at, _)| *at <= linked_at),
        "the beacon changed with the link: {adverts:?}"
    );
}
