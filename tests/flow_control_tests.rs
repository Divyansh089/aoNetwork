//! Credit-based flow control (NET-25): negotiation, waiting for credit,
//! granting back, a peer past its grant, and a slow receiver in the
//! simulator.

use std::time::Duration;

use ao_network::{
    AppHello, CAP_CREDIT, Channel, DisconnectReason, Fragmenter, Frame, Hello, Link, LinkAction,
    LinkEnd, LinkId, LinkRole, LocalTag, NetworkConfig, PeerKey, Reassembler, TransportEvent,
};

const T0: Duration = Duration::ZERO;
const MTU: u16 = 247;
const ID: LinkId = LinkId::new(1);
const CHANNELS: [Channel; 4] = [
    Channel::Control,
    Channel::Message,
    Channel::Telemetry,
    Channel::Bulk,
];

fn hello(caps: u32) -> Hello {
    Hello {
        proto_min: 1,
        proto_max: 1,
        caps,
        max_frame: 16 * 1024,
        tag: LocalTag::new([caps as u8; 16]),
        app_hello: AppHello::new(Vec::new()).unwrap(),
    }
}

/// Encoded size of a `Data` frame with `len` payload bytes below 16 KiB.
fn cost(len: usize) -> usize {
    3 + if len < 128 { 1 } else { 2 } + len
}

/// One `Link` driven by hand. Its writes complete only when the test
/// acknowledges them, so a grant can be held back unwritten.
struct End {
    link: Link,
    unacked: Vec<Vec<u8>>,
    peer_sees: Reassembler,
    /// Frames the link wrote, as the peer would decode them.
    sent: Vec<Frame>,
    got: Vec<(Channel, Vec<u8>)>,
    ended: Option<LinkEnd>,
}

impl End {
    fn new(config: &NetworkConfig, caps: u32) -> Self {
        let mut end = Self {
            link: Link::new(config, hello(caps)),
            unacked: Vec::new(),
            peer_sees: Reassembler::new(config),
            sent: Vec::new(),
            got: Vec::new(),
            ended: None,
        };
        let actions = end.link.on_event(
            T0,
            TransportEvent::Connected {
                link: ID,
                peer: PeerKey::new([2; 16]),
                role: LinkRole::Initiator,
                mtu: MTU,
            },
        );
        end.take(actions);
        end
    }

    /// Up, with our `Hello` written and the peer's received.
    fn ready(config: &NetworkConfig, ours: u32, theirs: u32) -> Self {
        let mut end = Self::new(config, ours);
        end.ack();
        end.feed(&Frame::Hello(hello(theirs)));
        end
    }

    fn take(&mut self, actions: Vec<LinkAction>) {
        for action in actions {
            match action {
                LinkAction::Write(chunk) => self.unacked.push(chunk),
                LinkAction::Data { channel, payload } => self.got.push((channel, payload)),
                LinkAction::Down(end) => self.ended = Some(end),
                _ => {}
            }
        }
    }

    /// Completes every write, including the ones each completion starts.
    fn ack(&mut self) {
        while !self.unacked.is_empty() {
            for chunk in std::mem::take(&mut self.unacked) {
                if let Some(bytes) = self.peer_sees.push(T0, &chunk).unwrap() {
                    self.sent.push(Frame::decode(&bytes, 16 * 1024).unwrap());
                }
                let actions = self
                    .link
                    .on_event(T0, TransportEvent::WriteReady { link: ID });
                self.take(actions);
            }
        }
    }

    fn feed(&mut self, frame: &Frame) {
        for chunk in Fragmenter::new(&frame.encode(), MTU).unwrap() {
            let actions = self
                .link
                .on_event(T0, TransportEvent::Received { link: ID, chunk });
            self.take(actions);
        }
    }

    fn send(&mut self, channel: Channel, len: usize) {
        let actions = self.link.send(T0, channel, vec![0x5A; len]).unwrap();
        self.take(actions);
    }

    fn credits(&self) -> Vec<(Channel, u32)> {
        self.sent
            .iter()
            .filter_map(|frame| match frame {
                Frame::Credit { channel, credits } => Some((*channel, *credits)),
                _ => None,
            })
            .collect()
    }

    fn data_sent(&self) -> Vec<Channel> {
        self.sent
            .iter()
            .filter_map(|frame| match frame {
                Frame::Data { channel, .. } => Some(*channel),
                _ => None,
            })
            .collect()
    }
}

/// Both `Hello`s must offer credits. Then each side grants its window per
/// channel at once and sends nothing without credit; otherwise no
/// `Credit` is sent and data goes out as before.
#[test]
fn credits_are_used_only_when_both_hellos_offer_them() {
    let config = NetworkConfig::default();
    for (ours, theirs) in [(CAP_CREDIT, CAP_CREDIT), (CAP_CREDIT, 0), (0, CAP_CREDIT)] {
        let both = ours & theirs != 0;
        let mut end = End::ready(&config, ours, theirs);
        end.send(Channel::Bulk, 100);
        end.ack();
        let windows = CHANNELS.map(|channel| {
            let window = config.receive_window(channel);
            (channel, u32::try_from(window).unwrap())
        });
        if both {
            assert_eq!(end.credits(), windows);
            assert!(end.data_sent().is_empty(), "sent without credit");
        } else {
            assert!(end.credits().is_empty(), "{ours} {theirs}");
            assert_eq!(end.data_sent(), [Channel::Bulk]);
        }
    }
}

/// A frame waits until its channel has credit for all of it, while a
/// channel that has credit goes ahead.
#[test]
fn a_frame_waits_until_its_channel_has_credit_for_all_of_it() {
    let mut end = End::ready(&NetworkConfig::default(), CAP_CREDIT, CAP_CREDIT);
    end.send(Channel::Bulk, 1000);
    end.send(Channel::Message, 10);
    end.ack();
    assert!(end.data_sent().is_empty());

    end.feed(&Frame::Credit {
        channel: Channel::Message,
        credits: cost(10) as u32,
    });
    end.ack();
    assert_eq!(end.data_sent(), [Channel::Message]);

    end.feed(&Frame::Credit {
        channel: Channel::Bulk,
        credits: cost(1000) as u32 - 1,
    });
    end.ack();
    assert_eq!(end.data_sent(), [Channel::Message], "one byte short");
    end.feed(&Frame::Credit {
        channel: Channel::Bulk,
        credits: 1,
    });
    end.ack();
    assert_eq!(end.data_sent(), [Channel::Message, Channel::Bulk]);
}

/// What the link hands on is granted back once half a window is owed,
/// in one `Credit` for all of it.
#[test]
fn processed_bytes_are_granted_back() {
    let config = NetworkConfig::default();
    let mut end = End::ready(&config, CAP_CREDIT, CAP_CREDIT);
    end.ack();
    let initial = end.credits().len();
    let half = config.receive_window(Channel::Bulk) / 2;
    let mut owed = 0;
    while owed + cost(1000) < half {
        end.feed(&Frame::Data {
            channel: Channel::Bulk,
            payload: vec![1; 1000],
        });
        end.ack();
        owed += cost(1000);
        assert_eq!(end.credits().len(), initial, "granted early at {owed}");
    }
    end.feed(&Frame::Data {
        channel: Channel::Bulk,
        payload: vec![1; 1000],
    });
    end.ack();
    owed += cost(1000);
    assert_eq!(
        end.credits()[initial..],
        [(Channel::Bulk, u32::try_from(owed).unwrap())]
    );
    assert_eq!(end.got.len(), owed / cost(1000));
}

/// A grant counts once it is written. A peer sending on a channel whose
/// grant still waits in the outbox could not have seen it, and is closed
/// as a protocol violation.
#[test]
fn a_peer_that_sends_past_its_grant_is_closed() {
    let config = NetworkConfig::default();
    let data = Frame::Data {
        channel: Channel::Bulk,
        payload: vec![1; 10],
    };

    // Grants written: the frame is within them.
    let mut granted = End::ready(&config, CAP_CREDIT, CAP_CREDIT);
    granted.ack();
    granted.feed(&data);
    assert_eq!(granted.got, [(Channel::Bulk, vec![1; 10])]);

    // Grants held back: only Control's, written first, has gone out.
    let mut held = End::ready(&config, CAP_CREDIT, CAP_CREDIT);
    held.feed(&data);
    assert!(held.got.is_empty());
    let actions = held.link.on_event(
        T0,
        TransportEvent::Disconnected {
            link: ID,
            reason: DisconnectReason::Closed,
        },
    );
    held.take(actions);
    assert_eq!(
        held.ended,
        Some(LinkEnd::Disconnected(DisconnectReason::ProtocolViolation))
    );
}

/// Done when: Bulk to a slow receiver never overruns its buffer. The
/// receiver holds at most a window of unread chunks, so with credits the
/// transfer completes on a buffer of that size; without them the same
/// transfer overruns it and the link is lost.
#[cfg(feature = "sim")]
#[test]
fn bulk_to_a_slow_receiver_never_overruns_its_buffer() {
    let window = NetworkConfig::default().receive_window(Channel::Bulk);
    // The window in frame bytes, a header byte per chunk, and room for the
    // few control frames (credits, keepalive) that share the buffer.
    let buffer = window + window.div_ceil(usize::from(MTU) - 4) + 256;

    let with = slow_transfer(NetworkConfig::default(), buffer);
    assert_eq!(with, Transfer::Complete);

    let without = NetworkConfig::default().with_flow_control(false).unwrap();
    assert_eq!(slow_transfer(without, buffer), Transfer::Lost);
}

#[cfg(feature = "sim")]
#[derive(Debug, PartialEq, Eq)]
enum Transfer {
    Complete,
    Lost,
}

/// Sends 256 KiB of Bulk from A to B, which has a receive buffer of
/// `buffer` bytes and polls only every 5 s. `receiver` configures B.
#[cfg(feature = "sim")]
fn slow_transfer(receiver: NetworkConfig, buffer: usize) -> Transfer {
    use std::task::{Context, Poll, Waker};

    use ao_network::{
        BeaconFlags, Network, NetworkEvent, SimConfig, SimNode, SimTransport, SimWorld,
    };

    const TICK: Duration = Duration::from_millis(10);
    const POLL_EVERY: u32 = 500;
    const TOTAL: usize = 256 * 1024;
    const PAYLOAD: usize = 4000;

    let world = SimWorld::new(SimConfig::default());
    let flags = BeaconFlags {
        accepting: true,
        ..BeaconFlags::default()
    };
    let node = |i: u8, config: NetworkConfig, rx_buffer| {
        let radio = world
            .add_node(SimNode {
                mtu: MTU,
                rx_buffer,
                ..SimNode::default()
            })
            .unwrap();
        let mut net: Network<SimTransport> = Network::new(config, radio);
        net.set_local_hello(LocalTag::new([i; 16]), AppHello::new(Vec::new()).unwrap());
        net.set_beacon([i; 8], [0; 4], flags);
        net
    };
    let mut a = node(1, NetworkConfig::default(), None);
    let mut b = node(2, receiver, Some(buffer));
    world.set_in_range(a.transport().node(), b.transport().node(), true);

    let mut cx = Context::from_waker(Waker::noop());
    let (mut queued, mut received) = (0, 0);
    let mut link = None;
    for tick in 0..60_000u32 {
        world.advance(TICK);
        let now = world.now();
        while let Poll::Ready(event) = a.poll_event(&mut cx, now) {
            match event {
                NetworkEvent::LinkUp { link: up, .. } => link = Some(up),
                NetworkEvent::LinkDown { .. } => return Transfer::Lost,
                _ => {}
            }
        }
        if let Some(link) = link {
            while queued < TOTAL {
                let len = PAYLOAD.min(TOTAL - queued);
                if a.send(now, link, Channel::Bulk, vec![0xB0; len]).is_err() {
                    break;
                }
                queued += len;
            }
        }
        // B reads only now and then: between reads, its radio holds what
        // arrives.
        if link.is_none() || tick % POLL_EVERY == 0 {
            while let Poll::Ready(event) = b.poll_event(&mut cx, now) {
                match event {
                    NetworkEvent::Data { payload, .. } => received += payload.len(),
                    NetworkEvent::LinkDown { .. } => return Transfer::Lost,
                    _ => {}
                }
            }
        }
        if received == TOTAL {
            return Transfer::Complete;
        }
    }
    panic!("neither complete nor lost: {received} of {TOTAL} bytes");
}
