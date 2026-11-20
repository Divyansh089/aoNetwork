//! The memory budget (NET-24): a node loaded to its worst case stays
//! within the ceilings written in the README.
//!
//! A counting allocator measures the heap the crate asks for. It counts
//! every thread, so this binary holds one test only. Skipped under Miri,
//! which runs it orders of magnitude slower and measures its own heap.
#![cfg(not(miri))]

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use ao_network::{
    AdvProfile, AppHello, Beacon, BeaconFlags, Channel, FRAME_VERSION, Fragmenter, Frame, Hello,
    LinkId, LinkRole, LocalTag, MAX_APP_HELLO, MIN_MTU, Network, NetworkConfig, PeerKey,
    ScanProfile, Transport, TransportCaps, TransportEvent,
};

// The ceilings in the README, at default config on a 64-bit target.
/// One link, fully loaded.
const LINK_BUDGET: usize = 288 * 1024;
/// A whole node: every link slot, the blacklist and the peer table.
const NODE_BUDGET: usize = 1536 * 1024;
/// `NativeTransport`'s pending events and scan results.
#[cfg(feature = "native")]
const NATIVE_BUDGET: usize = 640 * 1024;

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call forwards to `System` with the caller's own arguments,
// so `System` upholds the `GlobalAlloc` contract; the counters are atomics
// and never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's `layout` obligations pass through unchanged.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
            PEAK.fetch_max(live, Ordering::SeqCst);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `alloc` above with this `layout`.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

fn live() -> usize {
    LIVE.load(Ordering::SeqCst)
}

/// Restarts the peak at what is live now.
fn reset_peak() {
    PEAK.store(live(), Ordering::SeqCst);
}

fn peak() -> usize {
    PEAK.load(Ordering::SeqCst)
}

/// A radio that only does what the test scripts: it holds the events
/// queued for the node, discards writes, and confirms each disconnect.
struct Scripted {
    events: Radio,
    next_link: u64,
}

/// The events queued for the node, shared with the test that scripts them.
type Radio = Arc<Mutex<VecDeque<TransportEvent>>>;

impl Transport for Scripted {
    fn caps(&self) -> TransportCaps {
        TransportCaps {
            can_advertise: true,
            max_mtu: 517,
            max_links: 32,
        }
    }

    fn start(&mut self) {}

    fn stop(&mut self) {}

    fn set_advertising(&mut self, _: Option<AdvProfile>) {}

    fn set_scanning(&mut self, _: Option<ScanProfile>) {}

    fn connect(&mut self, _: PeerKey) -> LinkId {
        self.next_link += 1;
        LinkId::new(self.next_link)
    }

    fn disconnect(&mut self, link: LinkId) {
        self.events
            .lock()
            .unwrap()
            .push_back(TransportEvent::Disconnected {
                link,
                reason: ao_network::DisconnectReason::Closed,
            });
    }

    fn write(&mut self, _: LinkId, _: Vec<u8>) {}

    fn poll_event(&mut self, _: &mut Context<'_>) -> Poll<TransportEvent> {
        let next = self.events.lock().unwrap().pop_front();
        next.map_or(Poll::Pending, Poll::Ready)
    }
}

const NOW: Duration = Duration::from_secs(1);

struct Node {
    net: Network<Scripted>,
    radio: Radio,
    next_link: u64,
}

impl Node {
    fn new() -> Self {
        let radio = Radio::default();
        let transport = Scripted {
            events: Arc::clone(&radio),
            // Inbound ids come from the radio; keep them clear of outbound.
            next_link: 1 << 32,
        };
        let mut net = Network::new(NetworkConfig::default(), transport);
        net.set_local_hello(
            LocalTag::new([0xAA; 16]),
            AppHello::new(vec![0; MAX_APP_HELLO]).unwrap(),
        );
        net.set_beacon([0xAA; 8], [0; 4], BeaconFlags::default());
        Self {
            net,
            radio,
            next_link: 0,
        }
    }

    fn feed(&mut self, events: impl IntoIterator<Item = TransportEvent>) {
        let mut cx = Context::from_waker(Waker::noop());
        for event in events {
            self.radio.lock().unwrap().push_back(event);
            while self.net.poll_event(&mut cx, NOW).is_ready() {}
        }
    }

    /// An inbound link from `peer` at the smallest MTU, so every frame
    /// splits into the most chunks.
    fn connect_from(&mut self, peer: PeerKey) -> LinkId {
        self.next_link += 1;
        let link = LinkId::new(self.next_link);
        self.feed([TransportEvent::Connected {
            link,
            peer,
            role: LinkRole::Responder,
            mtu: MIN_MTU,
        }]);
        link
    }

    fn receive(&mut self, link: LinkId, frame: &Frame, all: bool) {
        let bytes = frame.encode();
        let mut chunks: Vec<Vec<u8>> = Fragmenter::new(&bytes, MIN_MTU).unwrap().collect();
        if !all {
            chunks.pop();
        }
        self.feed(
            chunks
                .into_iter()
                .map(|chunk| TransportEvent::Received { link, chunk }),
        );
    }

    fn writable(&mut self, link: LinkId, times: usize) {
        self.feed((0..times).map(|_| TransportEvent::WriteReady { link }));
    }
}

/// A peer key distinct per `group` and `i`.
fn key(group: u8, i: u8) -> PeerKey {
    let mut bytes = [group; 16];
    bytes[0] = i;
    PeerKey::new(bytes)
}

fn hello(tag: u8) -> Hello {
    Hello {
        proto_min: FRAME_VERSION,
        proto_max: FRAME_VERSION,
        caps: 0,
        max_frame: 16 * 1024,
        tag: LocalTag::new([tag; 16]),
        app_hello: AppHello::new(vec![tag; MAX_APP_HELLO]).unwrap(),
    }
}

/// The largest payload whose `Data` frame fits the default `max_frame`.
const LARGEST: usize = 16 * 1024 - 5;

/// What one run measured, in bytes.
struct Load {
    /// The most one link added, fully loaded.
    link: usize,
    /// The node's peak, from creation to fully loaded.
    node: usize,
}

/// Builds a node and loads it to the worst case it admits: a full
/// blacklist record table, every link slot taken, each link with its
/// queues full of `len`-byte payloads behind a largest frame being
/// written, and a largest frame half-received; then a full peer table.
fn load(len: usize) -> Load {
    let config = NetworkConfig::default();
    let max_peers = u8::try_from(config.max_peers()).unwrap();
    let max_links = u8::try_from(config.max_links()).unwrap();
    let base = live();
    reset_peak();
    let mut node = Node::new();

    // One strike each from as many peers as the guard tracks.
    for i in 0..max_peers {
        let link = node.connect_from(key(1, i));
        node.feed([TransportEvent::Received {
            link,
            chunk: vec![0xC0, 0xFF],
        }]);
    }

    let mut link_most = 0;
    for j in 0..max_links {
        let before = live();
        let link = node.connect_from(key(2, j));
        // Our Hello goes out; the peer's comes in.
        node.writable(link, 32);
        node.receive(link, &Frame::Hello(hello(j)), true);
        node.net
            .send(NOW, link, Channel::Bulk, vec![0x11; LARGEST])
            .unwrap();
        for channel in [
            Channel::Control,
            Channel::Message,
            Channel::Telemetry,
            Channel::Bulk,
        ] {
            // Telemetry evicts instead of refusing, so pushes are counted:
            // enough to fill any channel even if each payload were charged
            // only its frame, at least four bytes over its length.
            let pushes = config.channel_cap(channel) / (len + 4) + 2;
            for _ in 0..pushes {
                if node.net.send(NOW, link, channel, vec![0x22; len]).is_err() {
                    break;
                }
            }
        }
        let largest = Frame::Data {
            channel: Channel::Bulk,
            payload: vec![0x33; LARGEST],
        };
        node.receive(link, &largest, false);
        link_most = link_most.max(live() - before);
    }

    // More peers heard than the table keeps.
    for i in 0..max_peers.saturating_add(16) {
        let beacon = Beacon {
            flags: BeaconFlags::default(),
            degree: max_links,
            max_links,
            hint: [i; 8],
            digest: [i; 4],
        };
        node.feed([TransportEvent::Discovered {
            peer: key(3, i),
            rssi: -50,
            beacon: beacon.encode().to_vec(),
        }]);
    }
    let node_peak = peak() - base;

    // The load took hold: otherwise the budget would pass vacuously.
    let snapshot = node.net.snapshot(NOW);
    assert_eq!(snapshot.links.len(), usize::from(max_links), "len {len}");
    assert_eq!(snapshot.counters.strikes, u64::from(max_peers), "len {len}");
    assert_eq!(snapshot.peers, usize::from(max_peers), "len {len}");
    for link in &snapshot.links {
        assert!(link.queued_bytes > 0, "len {len}");
    }
    Load {
        link: link_most,
        node: node_peak,
    }
}

#[test]
fn a_loaded_node_stays_within_its_budget() {
    let mut worst = Load { link: 0, node: 0 };
    for len in [0, 1, 16, 64, 256, 1024, 4096, LARGEST] {
        let load = load(len);
        println!(
            "payloads of {len:>5} B: link {:>7} B, node {:>8} B",
            load.link, load.node
        );
        worst.link = worst.link.max(load.link);
        worst.node = worst.node.max(load.node);
    }
    println!("worst: link {} B, node {} B", worst.link, worst.node);
    assert!(
        worst.link <= LINK_BUDGET,
        "link {} > {LINK_BUDGET}",
        worst.link
    );
    assert!(
        worst.node <= NODE_BUDGET,
        "node {} > {NODE_BUDGET}",
        worst.node
    );

    #[cfg(feature = "native")]
    {
        let native = load_native();
        println!("native transport: {native} B");
        assert!(native <= NATIVE_BUDGET, "native {native} > {NATIVE_BUDGET}");
    }
}

/// Fills `NativeTransport`'s queues to their bounds: pending events of the
/// longest ATT value, and a scan result per peer it holds.
#[cfg(feature = "native")]
fn load_native() -> usize {
    use ao_network::{
        AdvertiseMode, NativeConfig, NativeEvents, NativeSink, NativeTransport, ScanMode,
    };

    struct Quiet;
    impl NativeSink for Quiet {
        fn set_advertising(&self, _: Option<[u8; 16]>, _: AdvertiseMode) {}
        fn set_scanning(&self, _: Option<ScanMode>) {}
        fn connect(&self, _: LinkId, _: PeerKey) {}
        fn write(&self, _: LinkId, _: Vec<u8>) {}
        fn disconnect(&self, _: LinkId) {}
    }

    let caps = TransportCaps {
        can_advertise: true,
        max_mtu: 517,
        max_links: 5,
    };
    let config = NativeConfig::new(caps);
    let base = live();
    reset_peak();
    let (mut transport, handle) = NativeTransport::new(Arc::new(Quiet), config);
    transport.start();
    let links: Vec<LinkId> = (0..5)
        .map(|i| {
            let link = transport.connect(key(4, i));
            handle.on_connected(link, 517);
            link
        })
        .collect();
    let events = config.max_pending_events.get();
    for n in 0..events {
        handle.on_bytes(links[n % links.len()], vec![0x44; 514]);
    }
    let scans = u8::try_from(config.max_pending_scans.get()).unwrap();
    for i in 0..scans.saturating_add(16) {
        handle.on_scan_result(key(5, i), -50, vec![0x55; 31]);
    }
    let native_peak = peak() - base;

    // The queues were full.
    let mut cx = Context::from_waker(Waker::noop());
    let (mut received, mut discovered) = (0, 0);
    while let Poll::Ready(event) = transport.poll_event(&mut cx) {
        match event {
            TransportEvent::Received { .. } => received += 1,
            TransportEvent::Discovered { .. } => discovered += 1,
            _ => {}
        }
    }
    assert_eq!(received + links.len(), events);
    assert_eq!(discovered, usize::from(scans));
    native_peak
}
