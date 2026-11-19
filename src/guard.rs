//! Inbound abuse protection (NET-18).
//!
//! Every neighbour is treated as hostile. Each link meters what its peer
//! sends with two [`TokenBucket`]s, one for frames and one for bytes, so a
//! peer that floods a link is cut off before it can tire the radio or the
//! layers above. The [`AbuseGuard`] remembers peers across links: a flood
//! blacklists the peer at once, and each malformed frame or broken chunk
//! stream is a strike, `max_strikes` of which blacklist it too. A
//! blacklisted peer is neither dialed nor accepted until its entry
//! expires. Everything is protocol-blind and runs on injected time.

use std::collections::HashMap;
use std::time::Duration;

use crate::{NetworkConfig, PeerKey};

/// Nanoseconds per second: bucket levels are kept in token-nanoseconds,
/// so refilling is one exact multiplication with no rounding to drift.
const NANOS: u64 = 1_000_000_000;

/// A sustained rate with a burst allowance, for one kind of inbound
/// traffic on one link.
///
/// The peer may send `burst` units at once, and `per_second` more each
/// second after that. Set through
/// [`NetworkConfig::with_frame_limit`] and
/// [`NetworkConfig::with_byte_limit`].
///
/// # Example
///
/// ```
/// use ao_network::{NetworkConfig, RateLimit};
///
/// let limit = RateLimit { per_second: 20, burst: 100 };
/// let config = NetworkConfig::default().with_frame_limit(limit)?;
/// assert_eq!(config.frame_limit(), limit);
/// # Ok::<(), ao_network::ConfigError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RateLimit {
    /// Units refilled per second.
    pub per_second: u32,
    /// Units the bucket holds when full: the largest burst allowed.
    pub burst: u32,
}

/// A token bucket on injected time. It starts full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TokenBucket {
    per_second: u64,
    capacity: u64,
    /// Tokens left, in token-nanoseconds.
    level: u64,
    /// When `level` was last brought up to date.
    at: Duration,
}

impl TokenBucket {
    /// A full bucket for `limit`, as of `now`.
    pub(crate) fn new(limit: RateLimit, now: Duration) -> Self {
        let capacity = u64::from(limit.burst).saturating_mul(NANOS);
        Self {
            per_second: u64::from(limit.per_second),
            capacity,
            level: capacity,
            at: now,
        }
    }

    /// Takes `units` at `now`, or returns `false` and takes nothing if the
    /// bucket does not hold that many. Time running backwards refills
    /// nothing.
    pub(crate) fn take(&mut self, now: Duration, units: u64) -> bool {
        let elapsed = u64::try_from(now.saturating_sub(self.at).as_nanos()).unwrap_or(u64::MAX);
        self.level = self
            .level
            .saturating_add(elapsed.saturating_mul(self.per_second))
            .min(self.capacity);
        self.at = self.at.max(now);
        let cost = units.saturating_mul(NANOS);
        if cost > self.level {
            return false;
        }
        self.level -= cost;
        true
    }
}

/// What the guard knows about one peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Record {
    strikes: u8,
    /// When the record lapses: strikes are forgotten, or the blacklist
    /// entry ends.
    until: Duration,
    blocked: bool,
}

/// Strikes and the temporary blacklist, keyed by radio peer.
///
/// Bounded by `max_peers`. When full, a lapsed record goes first, then the
/// unblocked record that lapses soonest, and only then the blacklist entry
/// that ends soonest, so a crowd of fresh addresses cannot push out an
/// active block cheaply.
#[derive(Debug)]
pub(crate) struct AbuseGuard {
    records: HashMap<PeerKey, Record>,
    max: usize,
    max_strikes: u8,
    duration: Duration,
}

impl AbuseGuard {
    pub(crate) fn new(config: &NetworkConfig) -> Self {
        Self {
            records: HashMap::new(),
            max: usize::try_from(config.max_peers()).unwrap_or(usize::MAX),
            max_strikes: config.max_strikes(),
            duration: config.blacklist_duration(),
        }
    }

    /// Whether `peer` is blacklisted at `now`.
    pub(crate) fn is_blocked(&self, peer: &PeerKey, now: Duration) -> bool {
        self.records
            .get(peer)
            .is_some_and(|record| record.blocked && now < record.until)
    }

    /// Records a strike against `peer` at `now` and returns whether the
    /// peer is now blacklisted. Strikes are forgotten `blacklist_duration`
    /// after the last one.
    pub(crate) fn strike(&mut self, now: Duration, peer: PeerKey) -> bool {
        let strikes = self
            .live(peer, now)
            .map_or(0, |record| record.strikes)
            .saturating_add(1);
        self.record(now, peer, strikes, strikes >= self.max_strikes)
    }

    /// Blacklists `peer` from `now` for `blacklist_duration`.
    pub(crate) fn block(&mut self, now: Duration, peer: PeerKey) {
        self.record(now, peer, self.max_strikes, true);
    }

    /// The record of `peer`, unless it has lapsed at `now`.
    fn live(&self, peer: PeerKey, now: Duration) -> Option<&Record> {
        self.records.get(&peer).filter(|record| now < record.until)
    }

    fn record(&mut self, now: Duration, peer: PeerKey, strikes: u8, blocked: bool) -> bool {
        // An existing block is never shortened or lifted by a strike.
        let blocked = blocked || self.is_blocked(&peer, now);
        let until = now.checked_add(self.duration).unwrap_or(Duration::MAX);
        self.records.retain(|_, record| now < record.until);
        if !self.records.contains_key(&peer) && self.records.len() >= self.max {
            let victim = self
                .records
                .iter()
                .min_by_key(|(key, record)| (record.blocked, record.until, **key))
                .map(|(&key, _)| key);
            if let Some(victim) = victim {
                self.records.remove(&victim);
            }
        }
        self.records.insert(
            peer,
            Record {
                strikes,
                until,
                blocked,
            },
        );
        blocked
    }

    /// The number of tracked peers; tests only.
    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.records.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEER: PeerKey = PeerKey::new([1; 16]);
    const MS: Duration = Duration::from_millis(1);

    fn guard() -> AbuseGuard {
        AbuseGuard::new(&NetworkConfig::default())
    }

    /// A full bucket gives its burst at once, then exactly its rate.
    #[test]
    fn a_bucket_allows_its_burst_then_its_rate() {
        let limit = RateLimit {
            per_second: 10,
            burst: 5,
        };
        let mut bucket = TokenBucket::new(limit, Duration::ZERO);
        for _ in 0..5 {
            assert!(bucket.take(Duration::ZERO, 1));
        }
        assert!(!bucket.take(Duration::ZERO, 1), "the burst is spent");
        // 10 per second is one per 100 ms, exactly.
        assert!(!bucket.take(99 * MS, 1));
        assert!(bucket.take(100 * MS, 1));
        assert!(!bucket.take(100 * MS, 1));
        // Refill stops at the burst.
        assert!(bucket.take(Duration::from_secs(60), 5));
        assert!(!bucket.take(Duration::from_secs(60), 1));
    }

    /// Refills in tiny steps add up exactly: nothing is lost to rounding.
    #[test]
    fn a_bucket_does_not_drift_over_small_steps() {
        // A burst of 2 keeps the level below the cap, which would
        // otherwise discard what accrues between a token and the poll.
        let limit = RateLimit {
            per_second: 3,
            burst: 2,
        };
        let mut bucket = TokenBucket::new(limit, Duration::ZERO);
        assert!(bucket.take(Duration::ZERO, 2));
        let mut now = Duration::ZERO;
        let mut taken = 0;
        // 3 per second over 10 s in 1 ms steps: exactly 30 tokens.
        for _ in 0..10_000 {
            now += Duration::from_millis(1);
            if bucket.take(now, 1) {
                taken += 1;
            }
        }
        assert_eq!(taken, 30);
    }

    /// A take larger than the bucket fails without draining it, and time
    /// going backwards neither refills nor panics.
    #[test]
    fn a_bucket_refuses_oversize_takes_and_backwards_time() {
        let limit = RateLimit {
            per_second: 1,
            burst: 4,
        };
        let mut bucket = TokenBucket::new(limit, Duration::from_secs(10));
        assert!(!bucket.take(Duration::from_secs(10), 5));
        assert!(bucket.take(Duration::from_secs(10), 4));
        assert!(!bucket.take(Duration::from_secs(1), 1));
        assert!(!bucket.take(Duration::from_secs(10), u64::MAX));
        assert!(bucket.take(Duration::from_secs(11), 1));
    }

    /// `max_strikes` strikes blacklist a peer; fewer do not.
    #[test]
    fn strikes_add_up_to_a_block() {
        let mut guard = guard();
        assert!(!guard.strike(Duration::ZERO, PEER));
        assert!(!guard.strike(Duration::from_secs(1), PEER));
        assert!(!guard.is_blocked(&PEER, Duration::from_secs(1)));
        assert!(guard.strike(Duration::from_secs(2), PEER));
        assert!(guard.is_blocked(&PEER, Duration::from_secs(2)));
    }

    /// Strikes are forgotten a blacklist duration after the last one, and
    /// a block ends after it.
    #[test]
    fn strikes_and_blocks_expire() {
        let mut guard = guard();
        let duration = NetworkConfig::default().blacklist_duration();
        guard.strike(Duration::ZERO, PEER);
        guard.strike(Duration::ZERO, PEER);
        // The third strike comes after the first two lapsed.
        assert!(!guard.strike(duration, PEER));

        guard.block(Duration::ZERO, PEER);
        assert!(guard.is_blocked(&PEER, duration - MS));
        assert!(!guard.is_blocked(&PEER, duration));
    }

    /// A strike against a blacklisted peer never lifts the block.
    #[test]
    fn a_strike_does_not_lift_a_block() {
        let mut guard = AbuseGuard::new(&NetworkConfig::default().with_max_strikes(5).unwrap());
        guard.block(Duration::ZERO, PEER);
        // A lapsed strike count would restart at one; the block holds.
        assert!(guard.strike(Duration::from_secs(1), PEER));
        assert!(guard.is_blocked(&PEER, Duration::from_secs(1)));
    }

    /// The guard never tracks more than `max_peers` peers, and a flood of
    /// fresh addresses with one strike each does not evict a block.
    #[test]
    fn the_guard_is_bounded_and_keeps_blocks() {
        let config = NetworkConfig::default().with_max_peers(4).unwrap();
        let mut guard = AbuseGuard::new(&config);
        guard.block(Duration::ZERO, PEER);
        for byte in 2..50u8 {
            guard.strike(Duration::ZERO, PeerKey::new([byte; 16]));
            assert!(guard.tracked() <= 4);
        }
        assert!(guard.is_blocked(&PEER, Duration::ZERO));

        // With only blocks left, the one ending soonest makes room.
        let mut guard = AbuseGuard::new(&config);
        for byte in 1..=4u8 {
            guard.block(
                Duration::from_secs(u64::from(byte)),
                PeerKey::new([byte; 16]),
            );
        }
        guard.block(Duration::from_secs(5), PeerKey::new([5; 16]));
        assert_eq!(guard.tracked(), 4);
        let now = Duration::from_secs(5);
        assert!(!guard.is_blocked(&PeerKey::new([1; 16]), now));
        assert!(guard.is_blocked(&PeerKey::new([2; 16]), now));
        assert!(guard.is_blocked(&PeerKey::new([5; 16]), now));
    }
}
