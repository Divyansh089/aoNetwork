//! `NativeTransport` against a Rust double of the Kotlin shim.
#![cfg(feature = "native")]

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use ao_network::{
    AppHello, BeaconFlags, Channel, ConnectError, DisconnectReason, LinkId, LinkRole, LocalTag,
    NativeConfig, NativeEvents, NativeHandle, NativeSink, NativeStatus, NativeTransport, Network,
    NetworkConfig, NetworkEvent, PeerKey, RadioState, ScanMode, Stage, Transport, TransportCaps,
    TransportEvent,
};

const CAPS: TransportCaps = TransportCaps {
    can_advertise: true,
    max_mtu: 517,
    max_links: 5,
};
const MTU: u16 = 185;

/// What Rust asked of Kotlin.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Advertise(Option<[u8; 16]>),
    Scan(Option<ScanMode>),
    Connect(LinkId, PeerKey),
    Write(LinkId, Vec<u8>),
    Disconnect(LinkId),
}

/// A `NativeSink` that records every call.
#[derive(Default)]
struct Recorder(Mutex<Vec<Call>>);

impl Recorder {
    fn take(&self) -> Vec<Call> {
        std::mem::take(&mut self.0.lock().unwrap())
    }
}

impl NativeSink for Recorder {
    fn set_advertising(&self, beacon: Option<[u8; 16]>, _: ao_network::AdvertiseMode) {
        self.0.lock().unwrap().push(Call::Advertise(beacon));
    }
    fn set_scanning(&self, mode: Option<ScanMode>) {
        self.0.lock().unwrap().push(Call::Scan(mode));
    }
    fn connect(&self, link: LinkId, peer: PeerKey) {
        self.0.lock().unwrap().push(Call::Connect(link, peer));
    }
    fn write(&self, link: LinkId, bytes: Vec<u8>) {
        self.0.lock().unwrap().push(Call::Write(link, bytes));
    }
    fn disconnect(&self, link: LinkId) {
        self.0.lock().unwrap().push(Call::Disconnect(link));
    }
}

fn native(config: NativeConfig) -> (NativeTransport, NativeHandle, Arc<Recorder>) {
    let recorder = Arc::new(Recorder::default());
    let (transport, handle) = NativeTransport::new(recorder.clone(), config);
    (transport, handle, recorder)
}

fn drain(transport: &mut NativeTransport) -> Vec<TransportEvent> {
    let mut cx = Context::from_waker(Waker::noop());
    let mut events = Vec::new();
    while let Poll::Ready(event) = transport.poll_event(&mut cx) {
        events.push(event);
    }
    events
}

/// A started transport with an outbound link that is up.
fn up_link() -> (NativeTransport, NativeHandle, Arc<Recorder>, LinkId) {
    let (mut t, handle, recorder) = native(NativeConfig::new(CAPS));
    t.start();
    let link = t.connect(PeerKey::new([2; 16]));
    handle.on_connected(link, MTU);
    drain(&mut t);
    recorder.take();
    (t, handle, recorder, link)
}

/// One phone: a `Network` on a `NativeTransport`, and the double's view.
struct Phone {
    net: Network<NativeTransport>,
    handle: NativeHandle,
    sink: Arc<Recorder>,
    key: PeerKey,
    /// The beacon it last advertised, which the other phone's scanner
    /// keeps reporting.
    advertised: Option<Vec<u8>>,
    events: Vec<NetworkEvent>,
}

impl Phone {
    fn new(i: u8) -> Self {
        let (transport, handle, sink) = native(NativeConfig::new(CAPS));
        handle.on_radio_state(RadioState::Ready);
        let mut net = Network::new(NetworkConfig::default(), transport);
        let app_hello = AppHello::new(vec![i]).unwrap();
        net.set_local_hello(LocalTag::new([i; 16]), app_hello);
        let flags = BeaconFlags {
            accepting: true,
            ..BeaconFlags::default()
        };
        net.set_beacon([i; 8], [0; 4], flags);
        Self {
            net,
            handle,
            sink,
            key: PeerKey::new([i; 16]),
            advertised: None,
            events: Vec::new(),
        }
    }
}

/// Plays Kotlin for two phones in range of each other: every sink call is
/// answered in order, as the contract's procedures would, until both are
/// quiet. `links` maps each side's link to the other side's.
fn pump(phones: &mut [Phone; 2], links: &mut BTreeMap<(usize, LinkId), LinkId>, now: Duration) {
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        let mut quiet = true;
        for me in 0..2 {
            let other = 1 - me;
            while let Poll::Ready(event) = phones[me].net.poll_event(&mut cx, now) {
                phones[me].events.push(event);
            }
            let mut advertised = None;
            for call in phones[me].sink.take() {
                quiet = false;
                let (key, peer) = (phones[me].key, &phones[other].handle);
                match call {
                    Call::Advertise(Some(beacon)) => {
                        peer.on_scan_result(key, -50, beacon.to_vec());
                        advertised = Some(beacon.to_vec());
                    }
                    Call::Advertise(None) | Call::Scan(_) => {}
                    Call::Connect(link, _) => {
                        let remote = peer.on_accepted(key, MTU);
                        phones[me].handle.on_connected(link, MTU);
                        links.insert((me, link), remote);
                        links.insert((other, remote), link);
                    }
                    Call::Write(link, bytes) => {
                        if let Some(&remote) = links.get(&(me, link)) {
                            peer.on_bytes(remote, bytes);
                        }
                        phones[me].handle.on_write_ready(link);
                    }
                    Call::Disconnect(link) => {
                        phones[me].handle.on_closed(link, NativeStatus::Requested);
                        if let Some(remote) = links.remove(&(me, link)) {
                            links.remove(&(other, remote));
                            let dropped = NativeStatus::Gatt {
                                stage: Stage::Connect,
                                code: 0x13,
                            };
                            peer.on_closed(remote, dropped);
                        }
                    }
                }
            }
            if advertised.is_some() {
                phones[me].advertised = advertised;
            }
        }
        if quiet {
            return;
        }
    }
}

#[test]
fn a_kotlin_double_drives_the_full_link_lifecycle() {
    let mut phones = [Phone::new(1), Phone::new(2)];
    let mut links = BTreeMap::new();
    let mut now = Duration::ZERO;
    // Returns the time it ran at, for commands issued after it.
    let mut step = |phones: &mut [Phone; 2]| {
        now += Duration::from_millis(10);
        pump(phones, &mut links, now);
        now
    };

    step(&mut phones);
    for phone in &mut phones {
        let events = std::mem::take(&mut phone.events);
        assert!(
            matches!(events[..], [NetworkEvent::LinkUp { .. }]),
            "{events:?}"
        );
    }
    let a_link = phones[0].net.links()[0].link;
    let b_link = phones[1].net.links()[0];
    assert_eq!(b_link.role, LinkRole::Responder);

    let at = step(&mut phones);
    phones[0]
        .net
        .send(at, a_link, Channel::Message, b"over the radio".to_vec())
        .unwrap();
    let at = step(&mut phones);
    assert_eq!(
        phones[1].events.pop(),
        Some(NetworkEvent::Data {
            link: b_link.link,
            channel: Channel::Message,
            payload: b"over the radio".to_vec()
        })
    );

    phones[0]
        .net
        .disconnect(at, a_link, DisconnectReason::DegreeCap);
    step(&mut phones);
    // Each side reports its reason, and the peer enters its backoff.
    for (phone, link, reason) in [
        (0, a_link, DisconnectReason::DegreeCap),
        (1, b_link.link, DisconnectReason::Bye),
    ] {
        let events = std::mem::take(&mut phones[phone].events);
        assert_eq!(events, [NetworkEvent::LinkDown { link, reason }]);
    }

    // Past the backoff, the scanners report each other again and the
    // pair comes back on a new link.
    for _ in 0..300 {
        step(&mut phones);
    }
    for me in 0..2 {
        let other = 1 - me;
        let beacon = phones[other].advertised.clone().unwrap();
        phones[me]
            .handle
            .on_scan_result(phones[other].key, -50, beacon);
    }
    step(&mut phones);
    for (phone, old) in [(0, a_link), (1, b_link.link)] {
        let events = std::mem::take(&mut phones[phone].events);
        assert!(
            matches!(events[..], [NetworkEvent::LinkUp { link: new, .. }] if new != old),
            "{events:?}"
        );
    }
}

#[test]
fn statuses_before_the_link_is_up_map_to_connect_errors() {
    let gatt = |stage, code| NativeStatus::Gatt { stage, code };
    let cases = [
        (gatt(Stage::Connect, 0x08), ConnectError::Timeout),
        (gatt(Stage::Connect, 0x22), ConnectError::Timeout),
        (gatt(Stage::Connect, 0x09), ConnectError::Rejected),
        (gatt(Stage::Connect, 0x0D), ConnectError::Rejected),
        (gatt(Stage::Connect, 0x13), ConnectError::Rejected),
        (gatt(Stage::Connect, 0x14), ConnectError::Rejected),
        (gatt(Stage::Connect, 0x15), ConnectError::Rejected),
        (gatt(Stage::Connect, 0x3E), ConnectError::Unreachable),
        (gatt(Stage::Connect, 0x85), ConnectError::Unreachable),
        (gatt(Stage::Discover, 0x85), ConnectError::SetupFailed),
        (gatt(Stage::Subscribe, 0x85), ConnectError::SetupFailed),
        (
            NativeStatus::Refused {
                stage: Stage::Connect,
                code: -1,
            },
            ConnectError::SetupFailed,
        ),
        (NativeStatus::ServiceMissing, ConnectError::ServiceMissing),
        (NativeStatus::PeerUnknown, ConnectError::PeerUnknown),
        (NativeStatus::RadioOff, ConnectError::RadioUnavailable),
        (
            NativeStatus::PermissionDenied,
            ConnectError::RadioUnavailable,
        ),
        // Kotlin claims a disconnect Rust never asked for.
        (NativeStatus::Requested, ConnectError::Unreachable),
    ];
    let (mut t, handle, _) = native(NativeConfig::new(CAPS));
    t.start();
    for (status, error) in cases {
        let link = t.connect(PeerKey::new([2; 16]));
        handle.on_closed(link, status);
        assert_eq!(
            drain(&mut t),
            [TransportEvent::ConnectFailed { link, error }],
            "{status:?}"
        );
    }
}

#[test]
fn statuses_after_the_link_is_up_map_to_disconnect_reasons() {
    let cases = [
        (
            NativeStatus::PeerMisbehaved,
            DisconnectReason::ProtocolViolation,
        ),
        (
            NativeStatus::Gatt {
                stage: Stage::Connect,
                code: 0x13,
            },
            DisconnectReason::TransportLost,
        ),
        (NativeStatus::RadioOff, DisconnectReason::TransportLost),
        (NativeStatus::Requested, DisconnectReason::TransportLost),
    ];
    for (status, reason) in cases {
        let (mut t, handle, _, link) = up_link();
        handle.on_closed(link, status);
        assert_eq!(
            drain(&mut t),
            [TransportEvent::Disconnected { link, reason }],
            "{status:?}"
        );
    }
}

#[test]
fn a_requested_close_ends_as_cancelled_or_closed() {
    let (mut t, handle, sink) = native(NativeConfig::new(CAPS));
    t.start();
    let link = t.connect(PeerKey::new([2; 16]));
    t.disconnect(link);
    // Setup finishing after the cancel is not reported.
    handle.on_connected(link, MTU);
    handle.on_closed(link, NativeStatus::Requested);
    assert_eq!(
        drain(&mut t),
        [TransportEvent::ConnectFailed {
            link,
            error: ConnectError::Cancelled
        }]
    );
    assert_eq!(sink.take().last(), Some(&Call::Disconnect(link)));

    let (mut t, handle, sink, link) = up_link();
    t.disconnect(link);
    // Whatever status the radio gives, a close Rust asked for is Closed.
    let lost = NativeStatus::Gatt {
        stage: Stage::Connect,
        code: 0x08,
    };
    handle.on_closed(link, lost);
    assert_eq!(
        drain(&mut t),
        [TransportEvent::Disconnected {
            link,
            reason: DisconnectReason::Closed
        }]
    );
    assert_eq!(sink.take(), [Call::Disconnect(link)]);
}

#[test]
fn late_and_unknown_callbacks_are_ignored() {
    let (mut t, handle, sink, link) = up_link();
    handle.on_closed(link, NativeStatus::RadioOff);
    drain(&mut t);
    handle.on_bytes(link, vec![1]);
    handle.on_write_ready(link);
    handle.on_mtu_changed(link, 100);
    handle.on_closed(link, NativeStatus::RadioOff);
    let stranger = LinkId::new(99);
    handle.on_connected(stranger, MTU);
    handle.on_bytes(stranger, vec![1]);
    assert!(drain(&mut t).is_empty());
    // Commands on the ended link are not passed to Kotlin.
    t.write(link, vec![1]);
    t.disconnect(link);
    assert!(sink.take().is_empty());
}

#[test]
fn one_write_in_flight_per_link() {
    let (mut t, handle, sink, link) = up_link();
    t.write(link, vec![1]);
    t.write(link, vec![2]);
    assert_eq!(sink.take(), [Call::Write(link, vec![1])]);
    handle.on_write_ready(link);
    assert_eq!(drain(&mut t), [TransportEvent::WriteReady { link }]);
    t.write(link, vec![3]);
    assert_eq!(sink.take(), [Call::Write(link, vec![3])]);
}

#[test]
fn nothing_happens_before_start_or_after_stop() {
    let (mut t, handle, sink) = native(NativeConfig::new(CAPS));
    handle.on_radio_state(RadioState::Ready);
    handle.on_scan_result(PeerKey::new([3; 16]), -50, vec![0; 16]);
    let early = t.connect(PeerKey::new([2; 16]));
    assert_eq!(
        drain(&mut t),
        [TransportEvent::ConnectFailed {
            link: early,
            error: ConnectError::RadioUnavailable
        }]
    );
    // An inbound link while stopped is closed and never reported.
    let inbound = handle.on_accepted(PeerKey::new([4; 16]), MTU);
    assert!(drain(&mut t).is_empty());
    assert_eq!(sink.take(), [Call::Disconnect(inbound)]);
    handle.on_closed(inbound, NativeStatus::Requested);

    // Start reports the radio state Kotlin gave earlier.
    t.start();
    assert_eq!(
        drain(&mut t),
        [TransportEvent::RadioState(RadioState::Ready)]
    );

    let link = t.connect(PeerKey::new([2; 16]));
    handle.on_connected(link, MTU);
    drain(&mut t);
    sink.take();
    t.stop();
    assert_eq!(
        sink.take(),
        [
            Call::Advertise(None),
            Call::Scan(None),
            Call::Disconnect(link)
        ]
    );
    handle.on_closed(link, NativeStatus::Requested);
    assert_eq!(
        drain(&mut t),
        [TransportEvent::Disconnected {
            link,
            reason: DisconnectReason::Closed
        }]
    );
    handle.on_scan_result(PeerKey::new([3; 16]), -50, vec![0; 16]);
    assert!(drain(&mut t).is_empty());
}

#[test]
fn dropping_the_transport_closes_links_and_silences_the_handle() {
    let (t, handle, sink, link) = up_link();
    drop(t);
    assert_eq!(sink.take().last(), Some(&Call::Disconnect(link)));
    handle.on_closed(link, NativeStatus::Requested);
    handle.on_radio_state(RadioState::Off);
    assert_eq!(
        handle.on_accepted(PeerKey::new([4; 16]), MTU),
        LinkId::new(0)
    );
}

#[test]
fn an_overflowing_link_is_closed_without_a_gap() {
    let mut config = NativeConfig::new(CAPS);
    config.max_pending_events = NonZeroUsize::new(4).unwrap();
    let (mut t, handle, sink) = native(config);
    t.start();
    let link = t.connect(PeerKey::new([2; 16]));
    handle.on_connected(link, MTU);
    for byte in 0..6 {
        handle.on_bytes(link, vec![byte]);
    }
    let received = |byte| TransportEvent::Received {
        link,
        chunk: vec![byte],
    };
    let events = drain(&mut t);
    assert!(matches!(events[0], TransportEvent::Connected { .. }));
    assert_eq!(events[1..], [received(0), received(1), received(2)]);
    assert_eq!(sink.take().last(), Some(&Call::Disconnect(link)));
    handle.on_closed(link, NativeStatus::Requested);
    assert_eq!(
        drain(&mut t),
        [TransportEvent::Disconnected {
            link,
            reason: DisconnectReason::TransportLost
        }]
    );
}

#[test]
fn scan_results_are_bounded_and_latest_wins() {
    let mut config = NativeConfig::new(CAPS);
    config.max_pending_scans = NonZeroUsize::new(2).unwrap();
    let (mut t, handle, _) = native(config);
    t.start();
    let peer = |i| PeerKey::new([i; 16]);
    handle.on_scan_result(peer(1), -80, vec![1; 16]);
    handle.on_scan_result(peer(1), -70, vec![2; 16]);
    handle.on_scan_result(peer(2), -60, vec![3; 16]);
    handle.on_scan_result(peer(3), -50, vec![4; 16]);
    // Longer than legacy advertising can carry.
    handle.on_scan_result(peer(1), -50, vec![5; 32]);
    let found = |p, rssi, b| TransportEvent::Discovered {
        peer: peer(p),
        rssi,
        beacon: vec![b; 16],
    };
    assert_eq!(drain(&mut t), [found(1, -70, 2), found(2, -60, 3)]);
}

#[test]
fn a_callback_wakes_the_poller() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;
    struct Count(AtomicUsize);
    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let count = Arc::new(Count(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&count));
    let (mut t, handle, _, link) = up_link();
    assert!(t.poll_event(&mut Context::from_waker(&waker)).is_pending());
    handle.on_bytes(link, vec![1]);
    assert_eq!(count.0.load(Ordering::SeqCst), 1);
}

#[test]
fn config_defaults() {
    let config = NativeConfig::new(CAPS);
    assert_eq!(config.caps, CAPS);
    assert_eq!(config.max_pending_events.get(), 1024);
    assert_eq!(config.max_pending_scans.get(), 64);
}
