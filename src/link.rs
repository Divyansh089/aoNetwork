//! One link's lifecycle: from the transport's `Connected` through the
//! `Hello` exchange to `Ready`, and down again with a typed reason.
//!
//! [`Link`] is sans-IO. The owner routes this link's [`TransportEvent`]s
//! in, submits payloads with [`Link::send`], calls [`Link::on_tick`] from
//! its timer, and carries out the returned [`LinkAction`]s. Time is
//! injected.

use std::collections::VecDeque;
use std::time::Duration;

use crate::codec::data_frame_len;
use crate::{
    Channel, ConnectError, DisconnectReason, Fragmenter, Frame, FrameError, HandshakeFailure,
    Hello, MAX_MTU, MIN_MTU, NetworkConfig, Reassembler, SendError, SendQueue, TransportEvent,
};

/// Where a link is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkState {
    /// Outbound link waiting for the transport to connect.
    Connecting,
    /// Connected; settling the MTU the link fragments for. The transports
    /// report a settled MTU in `Connected`, so this passes straight to
    /// [`Handshaking`](Self::Handshaking) in the same step.
    Negotiating,
    /// Our `Hello` is queued; waiting for a valid `Hello` from the peer.
    Handshaking,
    /// Both sides agreed on a version; data may flow.
    Ready,
    /// We are closing; waiting for the transport to confirm.
    Closing,
    /// Ended. No further actions.
    Closed,
}

/// Something the owner must do for this link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkAction {
    /// Write this chunk with `Transport::write`.
    Write(Vec<u8>),
    /// Close the transport link with `Transport::disconnect`.
    Disconnect,
    /// The handshake succeeded.
    Ready {
        /// The peer's `Hello`, as received.
        remote: Hello,
        /// The highest link protocol version both sides speak.
        version: u8,
    },
    /// A data frame arrived on a ready link.
    Data {
        /// Its channel.
        channel: Channel,
        /// Its payload.
        payload: Vec<u8>,
    },
    /// The link has ended. Always the last action.
    Down(LinkEnd),
}

/// How a link ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkEnd {
    /// It never connected.
    ConnectFailed(ConnectError),
    /// It connected and then went down.
    Disconnected(DisconnectReason),
}

/// The state machine for one link.
///
/// One outgoing frame is in flight at a time and one chunk of it per
/// transport write credit. Payloads wait in a [`SendQueue`] and the next
/// one is taken only when the frame before it is fully written, once the
/// link is ready. Payloads still queued when the link closes are discarded
/// with it. A peer frame other than `Hello` before the
/// handshake, a malformed frame or a broken chunk stream closes the link
/// with [`DisconnectReason::ProtocolViolation`].
#[derive(Debug)]
pub struct Link {
    local: Hello,
    max_frame: usize,
    handshake_timeout: Duration,
    state: LinkState,
    mtu: u16,
    deadline: Duration,
    reassembler: Reassembler,
    /// Chunks of the frames being written, in send order.
    outbox: VecDeque<Vec<u8>>,
    queue: SendQueue,
    can_write: bool,
    /// The peer's `max_frame`, once its `Hello` has arrived.
    remote_max_frame: Option<usize>,
    /// Why we are closing; reported when the transport confirms.
    closing: Option<DisconnectReason>,
    /// Disconnect once the outbox drains (a `Bye` is in it).
    disconnect_when_sent: bool,
}

impl Link {
    /// A link that will introduce itself with `local`. It starts in
    /// [`LinkState::Connecting`]; feed it the transport's `Connected`
    /// (outbound or inbound) to begin the handshake.
    pub fn new(config: &NetworkConfig, local: Hello) -> Self {
        Self {
            local,
            max_frame: config.max_frame(),
            handshake_timeout: config.handshake_timeout(),
            state: LinkState::Connecting,
            mtu: 0,
            deadline: Duration::ZERO,
            reassembler: Reassembler::new(config),
            outbox: VecDeque::new(),
            queue: SendQueue::new(config),
            can_write: false,
            remote_max_frame: None,
            closing: None,
            disconnect_when_sent: false,
        }
    }

    /// The current state.
    pub fn state(&self) -> LinkState {
        self.state
    }

    /// When [`on_tick`](Self::on_tick) next has work: the handshake
    /// deadline, or the reassembly deadline of a partial frame.
    pub fn next_deadline(&self) -> Option<Duration> {
        match self.state {
            LinkState::Negotiating | LinkState::Handshaking => Some(self.deadline),
            LinkState::Ready => self.reassembler.deadline(),
            _ => None,
        }
    }

    /// Queues `payload` on `channel`. Before the handshake completes it
    /// waits in the queue, unchecked against the peer's `max_frame`, which
    /// is not known yet.
    ///
    /// # Errors
    ///
    /// [`SendError::QueueFull`] if the channel is at its cap,
    /// [`SendError::TooLarge`] if its frame exceeds the peer's `max_frame`,
    /// or [`SendError::LinkClosed`] once the link is closing; each returns
    /// the payload unchanged.
    pub fn send(
        &mut self,
        channel: Channel,
        payload: Vec<u8>,
    ) -> Result<Vec<LinkAction>, SendError> {
        if matches!(self.state, LinkState::Closing | LinkState::Closed) {
            return Err(SendError::LinkClosed(payload));
        }
        if self
            .remote_max_frame
            .is_some_and(|max| data_frame_len(payload.len()) > max)
        {
            return Err(SendError::TooLarge(payload));
        }
        self.queue.push(channel, payload)?;
        let mut out = Vec::new();
        self.flush(&mut out);
        Ok(out)
    }

    /// Feeds one transport event for this link, received at `now`.
    /// Events for other links or about discovery are ignored.
    pub fn on_event(&mut self, now: Duration, event: TransportEvent) -> Vec<LinkAction> {
        let mut out = Vec::new();
        if self.state == LinkState::Closed {
            return out;
        }
        match event {
            TransportEvent::Connected { mtu, .. } if self.state == LinkState::Connecting => {
                // Negotiating ends at once: the MTU arrives settled.
                self.mtu = mtu;
                self.can_write = true;
                self.deadline = now + self.handshake_timeout;
                self.state = LinkState::Handshaking;
                if !self.mtu_ok(&mut out) {
                    return out;
                }
                self.enqueue(&Frame::Hello(self.local.clone()));
                self.flush(&mut out);
            }
            TransportEvent::MtuChanged { mtu, .. } if self.state != LinkState::Closing => {
                self.mtu = mtu;
                self.mtu_ok(&mut out);
            }
            TransportEvent::WriteReady { .. } => {
                self.can_write = true;
                self.flush(&mut out);
            }
            TransportEvent::Received { chunk, .. } => self.receive(now, &chunk, &mut out),
            TransportEvent::ConnectFailed { error, .. } => {
                self.end(LinkEnd::ConnectFailed(error), &mut out);
            }
            TransportEvent::Disconnected { reason, .. } => {
                let reason = self.closing.unwrap_or(reason);
                self.end(LinkEnd::Disconnected(reason), &mut out);
            }
            _ => {}
        }
        out
    }

    /// Runs the handshake and reassembly timers at `now`.
    pub fn on_tick(&mut self, now: Duration) -> Vec<LinkAction> {
        let mut out = Vec::new();
        let handshaking = matches!(self.state, LinkState::Negotiating | LinkState::Handshaking);
        if !handshaking && self.state != LinkState::Ready {
            return out;
        }
        if handshaking && now >= self.deadline {
            self.fail(
                DisconnectReason::HandshakeFailed(HandshakeFailure::Timeout),
                &mut out,
            );
        } else if self.reassembler.poll_timeout(now).is_err() {
            self.fail(DisconnectReason::ProtocolViolation, &mut out);
        }
        out
    }

    /// Closes the link for `reason`. A connected link says `Bye` first.
    pub fn close(&mut self, reason: DisconnectReason) -> Vec<LinkAction> {
        let mut out = Vec::new();
        match self.state {
            LinkState::Closing | LinkState::Closed => {}
            LinkState::Connecting => self.fail(reason, &mut out),
            _ => {
                self.closing = Some(reason);
                self.state = LinkState::Closing;
                self.enqueue(&Frame::Bye);
                self.disconnect_when_sent = true;
                self.flush(&mut out);
            }
        }
        out
    }

    fn receive(&mut self, now: Duration, chunk: &[u8], out: &mut Vec<LinkAction>) {
        if self.state == LinkState::Closing {
            return;
        }
        let bytes = match self.reassembler.push(now, chunk) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return,
            Err(_) => return self.fail(DisconnectReason::ProtocolViolation, out),
        };
        let frame = match Frame::decode(&bytes, self.max_frame) {
            Ok(frame) => frame,
            Err(FrameError::AppHelloTooLong { .. }) if self.state == LinkState::Handshaking => {
                return self.fail(
                    DisconnectReason::HandshakeFailed(HandshakeFailure::OversizeHello),
                    out,
                );
            }
            Err(_) => return self.fail(DisconnectReason::ProtocolViolation, out),
        };
        match (self.state, frame) {
            (LinkState::Handshaking, Frame::Hello(remote)) => match self.agree(&remote) {
                Some(version) => {
                    self.state = LinkState::Ready;
                    self.remote_max_frame =
                        Some(usize::try_from(remote.max_frame).unwrap_or(usize::MAX));
                    out.push(LinkAction::Ready { remote, version });
                    self.flush(out);
                }
                None => self.fail(
                    DisconnectReason::HandshakeFailed(HandshakeFailure::VersionMismatch),
                    out,
                ),
            },
            (LinkState::Ready, Frame::Data { channel, payload }) => {
                out.push(LinkAction::Data { channel, payload });
            }
            (LinkState::Ready | LinkState::Handshaking, Frame::Bye) => {
                self.fail(DisconnectReason::Bye, out);
            }
            // Keepalive (NET-13) and credits (NET-25) are not handled yet.
            (LinkState::Ready, Frame::Ping | Frame::Pong | Frame::Credit { .. }) => {}
            _ => self.fail(DisconnectReason::ProtocolViolation, out),
        }
    }

    /// The highest version in both ranges, if they overlap.
    fn agree(&self, remote: &Hello) -> Option<u8> {
        let low = self.local.proto_min.max(remote.proto_min);
        let high = self.local.proto_max.min(remote.proto_max);
        (low <= high).then_some(high)
    }

    /// Closes the link if the transport reported an MTU outside the BLE
    /// range: no frame could be fragmented for it.
    fn mtu_ok(&mut self, out: &mut Vec<LinkAction>) -> bool {
        let ok = (MIN_MTU..=MAX_MTU).contains(&self.mtu);
        if !ok {
            self.fail(DisconnectReason::TransportLost, out);
        }
        ok
    }

    fn enqueue(&mut self, frame: &Frame) {
        // Cannot fail: the MTU is checked whenever it changes.
        if let Ok(chunks) = Fragmenter::new(&frame.encode(), self.mtu) {
            self.outbox.extend(chunks);
        }
    }

    fn flush(&mut self, out: &mut Vec<LinkAction>) {
        if !self.can_write {
            return;
        }
        if self.outbox.is_empty() && self.state == LinkState::Ready {
            if let Some((channel, payload)) = self.queue.pop() {
                self.enqueue(&Frame::Data { channel, payload });
            }
        }
        if let Some(chunk) = self.outbox.pop_front() {
            self.can_write = false;
            out.push(LinkAction::Write(chunk));
        } else if self.disconnect_when_sent {
            self.disconnect_when_sent = false;
            out.push(LinkAction::Disconnect);
        }
    }

    /// Closes at once, without `Bye`, recording `reason` for the
    /// transport's confirmation.
    fn fail(&mut self, reason: DisconnectReason, out: &mut Vec<LinkAction>) {
        self.closing = Some(reason);
        self.state = LinkState::Closing;
        self.outbox.clear();
        self.disconnect_when_sent = false;
        out.push(LinkAction::Disconnect);
    }

    fn end(&mut self, end: LinkEnd, out: &mut Vec<LinkAction>) {
        self.state = LinkState::Closed;
        self.outbox.clear();
        out.push(LinkAction::Down(end));
    }
}
