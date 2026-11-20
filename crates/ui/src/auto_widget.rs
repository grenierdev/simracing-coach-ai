//! Automatic coaching widget: opened when iRacing starts and closed when it
//! stops, unless the driver took over.

#[derive(Debug, Default)]
pub struct AutoWidget {
    /// iRacing was running at the last update.
    running: bool,
    /// The widget was opened by this rule (not by the driver).
    opened_by_auto: bool,
}

impl AutoWidget {
    /// Returns `Some(true)` to open the widget, `Some(false)` to close it,
    /// `None` to leave it as it is. `open`: the widget is open.
    pub fn update(&mut self, iracing: bool, enabled: bool, open: bool) -> Option<bool> {
        let was = std::mem::replace(&mut self.running, iracing);
        if iracing && !was && enabled && !open {
            self.opened_by_auto = true;
            return Some(true);
        }
        if !iracing && was && std::mem::take(&mut self.opened_by_auto) {
            return open.then_some(false);
        }
        None
    }

    /// The option was just switched on: open now if iRacing is already running.
    pub fn enabled_now(&mut self, open: bool) -> Option<bool> {
        if self.running && !open {
            self.opened_by_auto = true;
            return Some(true);
        }
        None
    }

    /// The driver opened or closed the widget by hand: the rule no longer
    /// closes it.
    pub fn manual(&mut self) {
        self.opened_by_auto = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_when_iracing_starts_and_releases_when_it_stops() {
        let mut a = AutoWidget::default();
        assert_eq!(a.update(false, true, false), None);
        assert_eq!(a.update(true, true, false), Some(true));
        assert_eq!(a.update(true, true, true), None, "only on the transition");
        assert_eq!(a.update(false, true, true), Some(false));
        assert_eq!(a.update(false, true, false), None);
    }

    #[test]
    fn respects_the_option_and_the_driver() {
        let mut a = AutoWidget::default();
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
        let mut a = AutoWidget::default();
        assert_eq!(a.update(true, false, false), None);
        assert_eq!(a.enabled_now(false), Some(true));
        assert_eq!(a.update(false, true, true), Some(false));
        assert_eq!(a.enabled_now(false), None, "iRacing not running");
    }
}
