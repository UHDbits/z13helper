//! A deterministic debounce state machine for power-source changes.

/// Confirms a power-source value only after it has remained unchanged for the
/// configured delay. Time is supplied by the caller so this stays pure and
/// straightforward to test.
#[derive(Debug, Clone)]
pub struct PowerDebouncer {
    delay_ms: u64,
    pending: Option<(bool, u64)>,
    confirmed: Option<bool>,
}

impl PowerDebouncer {
    pub fn new(delay_ms: u64) -> Self {
        Self {
            delay_ms,
            pending: None,
            confirmed: None,
        }
    }

    pub fn set_delay(&mut self, delay_ms: u64) {
        self.delay_ms = delay_ms;
    }

    /// Feed the latest value and return it once when its deadline expires.
    ///
    /// Calling this periodically with the current state is intentional: it
    /// permits confirmation without needing a separate timer in the logic
    /// layer.
    pub fn on_signal(&mut self, on_battery: bool, now_ms: u64) -> Option<bool> {
        if self.confirmed == Some(on_battery) {
            self.pending = None;
            return None;
        }

        match self.pending {
            Some((value, deadline)) if value == on_battery && now_ms >= deadline => {
                self.pending = None;
                self.confirmed = Some(value);
                Some(value)
            }
            Some((value, _)) if value == on_battery => None,
            _ => {
                self.pending = Some((on_battery, now_ms.saturating_add(self.delay_ms)));
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PowerDebouncer;

    #[test]
    fn confirms_only_after_delay() {
        let mut d = PowerDebouncer::new(2000);
        assert_eq!(d.on_signal(true, 10), None);
        assert_eq!(d.on_signal(true, 2009), None);
        assert_eq!(d.on_signal(true, 2010), Some(true));
        assert_eq!(d.on_signal(true, 3000), None);
    }

    #[test]
    fn changed_signal_restarts_deadline() {
        let mut d = PowerDebouncer::new(100);
        assert_eq!(d.on_signal(true, 0), None);
        assert_eq!(d.on_signal(false, 50), None);
        assert_eq!(d.on_signal(false, 149), None);
        assert_eq!(d.on_signal(false, 150), Some(false));
    }
}
