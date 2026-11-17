//! Automatic always-on-top: the coaching window is pinned when iRacing
//! starts and released when it stops, unless the driver took over.

#[derive(Debug, Default)]
pub struct AutoPin {
    /// iRacing was running at the last update.
    running: bool,
    /// The current pin comes from this rule (not from the driver).
    pinned_by_auto: bool,
}

impl AutoPin {
    /// Returns `Some(true)` to pin, `Some(false)` to unpin, `None` to leave
    /// the window as it is.
    pub fn update(&mut self, iracing: bool, enabled: bool, pinned: bool) -> Option<bool> {
        let was = std::mem::replace(&mut self.running, iracing);
        if iracing && !was && enabled && !pinned {
            self.pinned_by_auto = true;
            return Some(true);
        }
        if !iracing && was && std::mem::take(&mut self.pinned_by_auto) {
            return pinned.then_some(false);
        }
        None
    }

    /// The option was just switched on: pin now if iRacing is already running.
    pub fn enabled_now(&mut self, pinned: bool) -> Option<bool> {
        if self.running && !pinned {
            self.pinned_by_auto = true;
            return Some(true);
        }
        None
    }

    /// The driver pinned or unpinned by hand: the rule no longer unpins.
    pub fn manual(&mut self) {
        self.pinned_by_auto = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_when_iracing_starts_and_releases_when_it_stops() {
        let mut a = AutoPin::default();
        assert_eq!(a.update(false, true, false), None);
        assert_eq!(a.update(true, true, false), Some(true));
        assert_eq!(a.update(true, true, true), None, "only on the transition");
        assert_eq!(a.update(false, true, true), Some(false));
        assert_eq!(a.update(false, true, false), None);
    }

    #[test]
    fn respects_the_option_and_the_driver() {
        let mut a = AutoPin::default();
        // Option off: nothing happens.
        assert_eq!(a.update(true, false, false), None);
        assert_eq!(a.update(false, false, false), None);
        // Already pinned by hand: not unpinned when iRacing stops.
        assert_eq!(a.update(true, true, true), None);
        assert_eq!(a.update(false, true, true), None);
        // Pinned by the rule, then unpinned by hand and pinned again by hand:
        // the driver's choice stays when iRacing stops.
        assert_eq!(a.update(true, true, false), Some(true));
        a.manual();
        assert_eq!(a.update(false, true, true), None);
    }

    #[test]
    fn switching_the_option_on_while_running_pins() {
        let mut a = AutoPin::default();
        assert_eq!(a.update(true, false, false), None);
        assert_eq!(a.enabled_now(false), Some(true));
        assert_eq!(a.update(false, true, true), Some(false));
        assert_eq!(a.enabled_now(false), None, "iRacing not running");
    }
}
