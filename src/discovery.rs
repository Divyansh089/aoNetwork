//! The discovery mode controller (NET-19).
//!
//! Discovery trades radio time against battery and against how far the
//! node is from its target degree. [`DiscoveryController`] maps those
//! inputs onto a [`DiscoveryMode`] and the scan and advertise profiles it
//! stands for, and never requests a profile change sooner than the
//! configured `discovery_interval` after the last one. Android throttles
//! apps that start scans in quick succession, so even the quiet mode's
//! scan duty cycle is built from long windows: scanning for one interval,
//! then pausing for [`QUIET_PAUSE_INTERVALS`] intervals.

use std::time::Duration;

use crate::{NetworkConfig, PowerMode};

/// How many intervals the quiet mode's scanner pauses between windows.
pub(crate) const QUIET_PAUSE_INTERVALS: u32 = 5;

/// How hard a node looks for neighbours.
///
/// Read with [`Network::discovery_mode`](crate::Network::discovery_mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum DiscoveryMode {
    /// Below the stable target degree: scan and advertise at low latency.
    Aggressive,
    /// At or above the target: balanced scanning and advertising.
    Normal,
    /// Low battery or idle: low-power advertising, and low-power scanning
    /// in windows of one interval separated by pauses of five.
    Quiet,
}

/// What the mode is decided from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Inputs {
    /// Links that are up.
    pub(crate) degree: usize,
    /// The degree below which the node is short of neighbours.
    pub(crate) target: usize,
    /// The caller's beacon says the battery is low.
    pub(crate) low_battery: bool,
    /// The caller says the node is idle.
    pub(crate) idle: bool,
}

impl DiscoveryMode {
    /// The mode `inputs` call for: quiet whenever the battery is low or
    /// the node is idle, otherwise aggressive below the target.
    pub(crate) fn for_inputs(inputs: &Inputs) -> Self {
        if inputs.low_battery || inputs.idle {
            Self::Quiet
        } else if inputs.degree < inputs.target {
            Self::Aggressive
        } else {
            Self::Normal
        }
    }
}

/// The radio profiles a mode asks for; `scan` is `None` while paused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Profiles {
    pub(crate) scan: Option<PowerMode>,
    pub(crate) advertise: PowerMode,
}

impl Profiles {
    fn steady(mode: DiscoveryMode) -> Self {
        let power = match mode {
            DiscoveryMode::Aggressive => PowerMode::LowLatency,
            DiscoveryMode::Normal => PowerMode::Balanced,
            DiscoveryMode::Quiet => PowerMode::LowPower,
        };
        Self {
            scan: Some(power),
            advertise: power,
        }
    }
}

/// Decides the mode and its profiles, rate-limited on injected time.
#[derive(Debug)]
pub(crate) struct DiscoveryController {
    interval: Duration,
    mode: DiscoveryMode,
    profiles: Profiles,
    /// When profiles were last handed to the radio, once known.
    requested_at: Option<Duration>,
    /// Profiles were handed to the radio by a command that had no time;
    /// the next poll stamps them with its own, which is no earlier.
    unstamped: bool,
    /// In the quiet mode, when the current scan window or pause ends.
    window_ends: Duration,
}

impl DiscoveryController {
    pub(crate) fn new(config: &NetworkConfig, inputs: &Inputs) -> Self {
        let mode = DiscoveryMode::for_inputs(inputs);
        Self {
            interval: config.discovery_interval(),
            mode,
            profiles: Profiles::steady(mode),
            requested_at: None,
            unstamped: false,
            window_ends: Duration::ZERO,
        }
    }

    /// The mode in force.
    pub(crate) fn mode(&self) -> DiscoveryMode {
        self.mode
    }

    /// The profiles in force.
    pub(crate) fn profiles(&self) -> Profiles {
        self.profiles
    }

    /// Records that the profiles in force were handed to the radio at
    /// `now`, or at an unknown time no later than the next poll.
    pub(crate) fn requested(&mut self, now: Option<Duration>) {
        match now {
            Some(now) => {
                self.requested_at = Some(now);
                self.unstamped = false;
            }
            None => self.unstamped = true,
        }
    }

    /// Moves to the mode `inputs` call for at `now`, or to the next quiet
    /// window, unless the last request was less than an interval ago.
    /// Returns whether the profiles changed; the caller hands them to the
    /// radio and reports it with [`requested`](Self::requested).
    pub(crate) fn poll(&mut self, now: Duration, inputs: &Inputs) -> bool {
        if self.unstamped {
            self.requested(Some(now));
        }
        if self.ready_at().is_some_and(|ready| now < ready) {
            return false;
        }
        let mode = DiscoveryMode::for_inputs(inputs);
        let profiles = match (self.mode, mode) {
            (DiscoveryMode::Quiet, DiscoveryMode::Quiet) if now < self.window_ends => self.profiles,
            (DiscoveryMode::Quiet, DiscoveryMode::Quiet) => {
                let scanning = self.profiles.scan.is_none();
                self.window_ends = now.saturating_add(self.window(scanning));
                Profiles {
                    scan: scanning.then_some(PowerMode::LowPower),
                    ..self.profiles
                }
            }
            (_, DiscoveryMode::Quiet) => {
                self.window_ends = now.saturating_add(self.window(true));
                Profiles::steady(mode)
            }
            _ => Profiles::steady(mode),
        };
        self.mode = mode;
        let changed = profiles != self.profiles;
        self.profiles = profiles;
        changed
    }

    /// When [`poll`](Self::poll) next has work for `inputs`: a mode change
    /// waiting out the interval, or the end of a quiet window. A request
    /// still to be stamped is due at once.
    pub(crate) fn next_deadline(&self, inputs: &Inputs) -> Option<Duration> {
        if self.unstamped {
            return Some(Duration::ZERO);
        }
        let ready = self.ready_at().unwrap_or(Duration::ZERO);
        if DiscoveryMode::for_inputs(inputs) != self.mode {
            Some(ready)
        } else if self.mode == DiscoveryMode::Quiet {
            Some(self.window_ends.max(ready))
        } else {
            None
        }
    }

    /// The earliest time another request may be made.
    fn ready_at(&self) -> Option<Duration> {
        self.requested_at
            .map(|at| at.checked_add(self.interval).unwrap_or(Duration::MAX))
    }

    /// How long a quiet scan window, or the pause after it, lasts.
    fn window(&self, scanning: bool) -> Duration {
        if scanning {
            self.interval
        } else {
            self.interval
                .checked_mul(QUIET_PAUSE_INTERVALS)
                .unwrap_or(Duration::MAX)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn inputs(degree: usize, low_battery: bool, idle: bool) -> Inputs {
        Inputs {
            degree,
            target: 3,
            low_battery,
            idle,
        }
    }

    /// A controller whose first request went out at time zero.
    fn started(at: &Inputs) -> DiscoveryController {
        let mut controller = DiscoveryController::new(&NetworkConfig::default(), at);
        controller.requested(Some(Duration::ZERO));
        controller
    }

    /// Polls at `now` and, like the network, reports any change as
    /// handed to the radio at once.
    fn poll(controller: &mut DiscoveryController, now: Duration, at: &Inputs) -> bool {
        let changed = controller.poll(now, at);
        if changed {
            controller.requested(Some(now));
        }
        changed
    }

    /// Done when: the mode follows the degree and battery inputs, and
    /// each mode has its profiles.
    #[test]
    fn modes_follow_degree_and_battery() {
        let mut c = started(&inputs(0, false, false));
        assert_eq!(c.mode(), DiscoveryMode::Aggressive);
        let fast = PowerMode::LowLatency;
        assert_eq!(
            c.profiles(),
            Profiles {
                scan: Some(fast),
                advertise: fast
            }
        );

        // Reaching the target.
        assert!(poll(&mut c, secs(10), &inputs(3, false, false)));
        assert_eq!(c.mode(), DiscoveryMode::Normal);
        let balanced = PowerMode::Balanced;
        assert_eq!(
            c.profiles(),
            Profiles {
                scan: Some(balanced),
                advertise: balanced
            }
        );

        // The battery runs low: quiet, even though above target.
        assert!(poll(&mut c, secs(20), &inputs(4, true, false)));
        assert_eq!(c.mode(), DiscoveryMode::Quiet);
        let low = PowerMode::LowPower;
        assert_eq!(
            c.profiles(),
            Profiles {
                scan: Some(low),
                advertise: low
            }
        );

        // Low battery wins over being below the target.
        assert_eq!(
            DiscoveryMode::for_inputs(&inputs(0, true, false)),
            DiscoveryMode::Quiet
        );

        // Charged again and short of links: aggressive.
        assert!(poll(&mut c, secs(40), &inputs(0, false, false)));
        assert_eq!(c.mode(), DiscoveryMode::Aggressive);

        // Idle is quiet too.
        assert!(poll(&mut c, secs(50), &inputs(0, false, true)));
        assert_eq!(c.mode(), DiscoveryMode::Quiet);
    }

    /// Done when: no profile change is requested sooner than the
    /// interval after the last, however fast the inputs flap; the change
    /// lands exactly when the interval ends, and the deadline says so.
    #[test]
    fn changes_wait_out_the_interval() {
        let below = inputs(0, false, false);
        let at_target = inputs(3, false, false);
        let mut c = started(&below);
        for ms in (0..10_000).step_by(250) {
            let now = Duration::from_millis(ms);
            let flap = if ms % 500 == 0 { &at_target } else { &below };
            assert!(!poll(&mut c, now, flap), "changed at {now:?}");
        }
        assert_eq!(c.next_deadline(&at_target), Some(secs(10)));
        assert!(poll(&mut c, secs(10), &at_target));
        assert_eq!(c.mode(), DiscoveryMode::Normal);
        // Back below: due one interval after this change, not before.
        assert_eq!(c.next_deadline(&below), Some(secs(20)));
        assert!(!poll(&mut c, secs(20) - Duration::from_millis(1), &below));
        assert!(poll(&mut c, secs(20), &below));
        // Nothing pending when the inputs agree with the mode.
        assert_eq!(c.next_deadline(&below), None);
    }

    /// Quiet scans in long windows: one interval on, five off, never
    /// toggling faster, while advertising stays put.
    #[test]
    fn quiet_scans_in_long_windows() {
        let quiet = inputs(0, true, false);
        let mut c = started(&inputs(0, false, false));
        assert!(poll(&mut c, secs(10), &quiet));
        let mut changes = vec![(secs(10), c.profiles().scan)];
        for s in 11..=130 {
            if poll(&mut c, secs(s), &quiet) {
                changes.push((secs(s), c.profiles().scan));
                assert_eq!(c.profiles().advertise, PowerMode::LowPower);
            }
        }
        let low = Some(PowerMode::LowPower);
        assert_eq!(
            changes,
            [
                (secs(10), low),
                (secs(20), None),
                (secs(70), low),
                (secs(80), None),
                (secs(130), low),
            ]
        );
        assert_eq!(c.next_deadline(&quiet), Some(secs(140)));
    }

    /// A request made without a time is stamped at the next poll, which
    /// is no earlier than the request, and is due at once until then.
    #[test]
    fn an_unstamped_request_is_timed_by_the_next_poll() {
        let below = inputs(0, false, false);
        let mut c = DiscoveryController::new(&NetworkConfig::default(), &below);
        // Nothing requested yet: a change is free.
        assert!(poll(&mut c, secs(1), &inputs(3, false, false)));
        c.requested(None);
        assert_eq!(c.next_deadline(&below), Some(Duration::ZERO));
        assert!(!poll(&mut c, secs(5), &below), "stamped at 5 s");
        assert_eq!(c.next_deadline(&below), Some(secs(15)));
        assert!(poll(&mut c, secs(15), &below));
    }
}
