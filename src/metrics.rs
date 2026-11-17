//! Per-link counters and moving averages.

use std::time::Duration;

/// Weight of a new sample in the moving averages: 1/8, as for TCP's
/// smoothed RTT (RFC 6298), so one outlier moves the average little.
const EWMA_SHIFT: u32 = 3;

/// What one link has seen so far. Bytes are counted as chunks on the
/// radio, headers included.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct LinkMetrics {
    /// Moving average of RSSI readings in dBm, if the radio reports any.
    pub rssi: Option<i16>,
    /// Moving average of ping round trips, once a pong has arrived.
    pub rtt: Option<Duration>,
    /// Bytes written.
    pub tx_bytes: u64,
    /// Bytes received.
    pub rx_bytes: u64,
    /// Received frames discarded without closing the link: credits, which
    /// are reserved, and pongs no ping asked for.
    pub dropped_frames: u64,
    /// When the last chunk arrived.
    pub last_rx: Option<Duration>,
}

impl LinkMetrics {
    pub(crate) fn on_rssi(&mut self, sample: i16) {
        self.rssi = Some(match self.rssi {
            // Widened so the difference cannot overflow; the result lies
            // between the old average and the sample, so it fits in i16.
            Some(avg) => {
                let step = (i32::from(sample) - i32::from(avg)) >> EWMA_SHIFT;
                i16::try_from(i32::from(avg) + step).unwrap_or(avg)
            }
            None => sample,
        });
    }

    pub(crate) fn on_rtt(&mut self, sample: Duration) {
        self.rtt = Some(match self.rtt {
            Some(avg) => avg - avg / (1 << EWMA_SHIFT) + sample / (1 << EWMA_SHIFT),
            None => sample,
        });
    }

    pub(crate) fn on_tx(&mut self, bytes: usize) {
        self.tx_bytes = self.tx_bytes.saturating_add(bytes as u64);
    }

    pub(crate) fn on_rx(&mut self, now: Duration, bytes: usize) {
        self.rx_bytes = self.rx_bytes.saturating_add(bytes as u64);
        self.last_rx = Some(now);
    }

    pub(crate) fn on_dropped(&mut self) {
        self.dropped_frames = self.dropped_frames.saturating_add(1);
    }
}
