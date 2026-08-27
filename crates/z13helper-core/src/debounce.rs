//! A deterministic debounce state machine for power-source changes.

/// The observation delivered by a power-source owner.
///
/// `Unknown` is deliberately not an AC value. It invalidates an outstanding
/// confirmation and leaves the last confirmed value untouched.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PowerObservation {
    Known(bool),
    Unknown,
}

/// The token owned by one forced confirmation timer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfirmationTimer {
    pub generation: u64,
    pub deadline_ms: u64,
}

/// Work requested by an observation or by a timer callback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DebounceAction {
    Ignored,
    Wait(ConfirmationTimer),
    Confirmed(bool),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Pending {
    value: bool,
    started_at_ms: u64,
    generation: u64,
}

/// Confirms a power-source value only after it has remained unchanged for the
/// configured delay. Time is supplied by the caller so this stays pure and
/// straightforward to test. Callers must provide a monotonic millisecond
/// clock; the state machine never reads the system clock itself.
#[derive(Debug, Clone)]
pub struct PowerDebouncer {
    delay_ms: u64,
    pending: Option<Pending>,
    confirmed: Option<bool>,
    generation: u64,
}

impl PowerDebouncer {
    pub fn new(delay_ms: u64) -> Self {
        Self {
            delay_ms,
            pending: None,
            confirmed: None,
            generation: 0,
        }
    }

    pub fn set_delay(&mut self, delay_ms: u64) {
        self.delay_ms = delay_ms;
    }

    /// Feed an observation. A repeated known value keeps the original
    /// deadline instead of postponing it; a reversal starts a new generation.
    pub fn observe(&mut self, observation: PowerObservation, now_ms: u64) -> DebounceAction {
        let PowerObservation::Known(value) = observation else {
            self.invalidate_pending();
            return DebounceAction::Ignored;
        };

        if self.confirmed == Some(value) {
            self.pending = None;
            return DebounceAction::Ignored;
        }

        let pending = match self.pending {
            Some(pending) if pending.value == value => pending,
            _ => {
                let pending = Pending {
                    value,
                    started_at_ms: now_ms,
                    generation: self.next_generation(),
                };
                self.pending = Some(pending);
                pending
            }
        };

        self.action_for_pending(pending, now_ms)
    }

    /// Preserve the small legacy convenience API for callers that only need
    /// to feed a signal. Timer-aware callers should use [`Self::observe`] and
    /// [`Self::on_timer`].
    pub fn on_signal(&mut self, on_battery: bool, now_ms: u64) -> Option<bool> {
        match self.observe(PowerObservation::Known(on_battery), now_ms) {
            DebounceAction::Confirmed(value) => Some(value),
            DebounceAction::Ignored | DebounceAction::Wait(_) => None,
        }
    }

    /// Force confirmation for one timer generation. Stale, canceled, or
    /// early callbacks are harmless; an early callback receives the current
    /// token so the UI can schedule the remaining delay.
    pub fn on_timer(&mut self, timer: ConfirmationTimer, now_ms: u64) -> DebounceAction {
        let Some(pending) = self.pending else {
            return DebounceAction::Ignored;
        };
        if pending.generation != timer.generation {
            return DebounceAction::Ignored;
        }
        self.action_for_pending(pending, now_ms)
    }

    /// Accept a value already verified by an authoritative resume sample.
    /// Advancing the generation also invalidates callbacks belonging to the
    /// pre-suspend observation.
    pub fn force_confirm(&mut self, value: bool) -> DebounceAction {
        self.pending = None;
        self.next_generation();
        self.confirmed = Some(value);
        DebounceAction::Confirmed(value)
    }

    /// Cancel an unconfirmed transition when the observation source becomes
    /// unknown. The last confirmed state remains authoritative.
    pub fn invalidate_pending(&mut self) {
        if self.pending.take().is_some() {
            self.next_generation();
        }
    }

    pub fn confirmed(&self) -> Option<bool> {
        self.confirmed
    }

    fn action_for_pending(&mut self, pending: Pending, now_ms: u64) -> DebounceAction {
        let deadline_ms = pending.started_at_ms.saturating_add(self.delay_ms);
        let timer = ConfirmationTimer {
            generation: pending.generation,
            deadline_ms,
        };
        if now_ms < deadline_ms {
            return DebounceAction::Wait(timer);
        }

        self.pending = None;
        self.confirmed = Some(pending.value);
        DebounceAction::Confirmed(pending.value)
    }

    fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfirmationTimer, DebounceAction, PowerDebouncer, PowerObservation};

    #[test]
    fn confirms_only_after_delay() {
        let mut d = PowerDebouncer::new(2000);
        let timer = match d.observe(PowerObservation::Known(true), 10) {
            DebounceAction::Wait(timer) => timer,
            other => panic!("expected timer, got {other:?}"),
        };
        assert_eq!(timer.deadline_ms, 2010);
        assert!(matches!(d.on_timer(timer, 2009), DebounceAction::Wait(_)));
        assert_eq!(d.on_timer(timer, 2010), DebounceAction::Confirmed(true));
        assert_eq!(d.confirmed(), Some(true));
    }

    #[test]
    fn repeated_signal_does_not_postpone_confirmation() {
        let mut d = PowerDebouncer::new(100);
        let first = match d.observe(PowerObservation::Known(true), 0) {
            DebounceAction::Wait(timer) => timer,
            other => panic!("expected timer, got {other:?}"),
        };
        assert_eq!(
            d.observe(PowerObservation::Known(true), 50),
            DebounceAction::Wait(first)
        );
        assert_eq!(d.on_timer(first, 100), DebounceAction::Confirmed(true));
    }

    #[test]
    fn reversal_starts_a_new_generation() {
        let mut d = PowerDebouncer::new(100);
        let old = match d.observe(PowerObservation::Known(true), 0) {
            DebounceAction::Wait(timer) => timer,
            other => panic!("expected timer, got {other:?}"),
        };
        let new = match d.observe(PowerObservation::Known(false), 50) {
            DebounceAction::Wait(timer) => timer,
            other => panic!("expected timer, got {other:?}"),
        };
        assert_ne!(old.generation, new.generation);
        assert_eq!(d.on_timer(old, 100), DebounceAction::Ignored);
        assert_eq!(d.on_timer(new, 150), DebounceAction::Confirmed(false));
    }

    #[test]
    fn unknown_invalidates_timer_without_changing_confirmation() {
        let mut d = PowerDebouncer::new(100);
        let timer = match d.observe(PowerObservation::Known(true), 0) {
            DebounceAction::Wait(timer) => timer,
            other => panic!("expected timer, got {other:?}"),
        };
        assert_eq!(
            d.observe(PowerObservation::Unknown, 50),
            DebounceAction::Ignored
        );
        assert_eq!(d.on_timer(timer, 100), DebounceAction::Ignored);
        assert_eq!(d.confirmed(), None);
    }

    #[test]
    fn stale_timer_is_ignored_after_confirmation() {
        let mut d = PowerDebouncer::new(0);
        assert_eq!(
            d.observe(PowerObservation::Known(true), u64::MAX),
            DebounceAction::Confirmed(true)
        );
        assert_eq!(
            d.on_timer(
                ConfirmationTimer {
                    generation: 1,
                    deadline_ms: u64::MAX,
                },
                u64::MAX
            ),
            DebounceAction::Ignored
        );
    }

    #[test]
    fn forced_confirmation_invalidates_the_previous_timer_generation() {
        let mut d = PowerDebouncer::new(100);
        let old = match d.observe(PowerObservation::Known(true), 0) {
            DebounceAction::Wait(timer) => timer,
            other => panic!("expected timer, got {other:?}"),
        };
        assert_eq!(d.force_confirm(false), DebounceAction::Confirmed(false));
        assert_eq!(d.on_timer(old, 100), DebounceAction::Ignored);
        assert_eq!(d.confirmed(), Some(false));
    }

    #[test]
    fn deadline_addition_saturates_at_u64_max() {
        let mut d = PowerDebouncer::new(u64::MAX);
        let timer = match d.observe(PowerObservation::Known(true), u64::MAX - 1) {
            DebounceAction::Wait(timer) => timer,
            other => panic!("expected timer, got {other:?}"),
        };
        assert_eq!(timer.deadline_ms, u64::MAX);
        assert_eq!(d.on_timer(timer, u64::MAX - 1), DebounceAction::Wait(timer));
        assert_eq!(d.on_timer(timer, u64::MAX), DebounceAction::Confirmed(true));
    }

    #[test]
    fn changing_delay_updates_pending_deadline() {
        let mut d = PowerDebouncer::new(100);
        assert!(matches!(
            d.observe(PowerObservation::Known(true), 0),
            DebounceAction::Wait(_)
        ));
        d.set_delay(200);
        assert!(matches!(
            d.observe(PowerObservation::Known(true), 199),
            DebounceAction::Wait(ConfirmationTimer {
                deadline_ms: 200,
                ..
            })
        ));
        assert_eq!(d.on_signal(true, 200), Some(true));
    }
}
