//! Input devices read in the background while iRacing has focus.
//!
//! Sources:
//! - keyboard and mouse: global state (`device_query`: `GetAsyncKeyState` on
//!   Windows, X11 on Linux);
//! - wheels, button boxes, joysticks, gamepads: DirectInput in non-exclusive
//!   background mode on Windows (the API iRacing uses; Windows Gaming Input
//!   requires focus), `gilrs` (evdev) on Linux.
//!
//! The service polls every source at 200 Hz and computes edges (press,
//! release). Two uses: capturing a binding ("press a key, Escape to cancel")
//! and tracking the push-to-talk button.

mod backend;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use serde::{Deserialize, Serialize};

pub use backend::Backend;

/// An identifiable input: key, mouse button or device button.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Binding {
    /// Keyboard key (`device_query` name, e.g. "F13", "Space", "LControl").
    Key { key: String },
    /// Mouse button (1 = left, 2 = right, 3 = middle, 4 and 5 = side).
    Mouse { button: u8 },
    /// Button of a game device.
    Controller {
        /// Product name, displayed and used if the identifier no longer matches.
        device: String,
        /// Instance identifier (DirectInput GUID or gilrs UUID).
        instance: String,
        button: u16,
    },
    /// Hat (POV) direction: 0 up, 1 right, 2 down, 3 left.
    Hat {
        device: String,
        instance: String,
        hat: u8,
        direction: u8,
    },
}

impl Binding {
    pub fn label(&self) -> String {
        match self {
            Binding::Key { key } => format!("Keyboard: {key}"),
            Binding::Mouse { button } => format!("Mouse: button {button}"),
            Binding::Controller { device, button, .. } => {
                format!("{device}: button {}", button + 1)
            }
            Binding::Hat {
                device,
                hat,
                direction,
                ..
            } => {
                let d = ["up", "right", "down", "left"]
                    .get(*direction as usize)
                    .copied()
                    .unwrap_or("?");
                format!("{device}: hat {} {d}", hat + 1)
            }
        }
    }

    /// Same physical input. For a device, the instance identifier takes precedence;
    /// if it changed (different USB port), the product name is enough.
    pub fn matches(&self, other: &Binding) -> bool {
        match (self, other) {
            (
                Binding::Controller {
                    device: d1,
                    instance: i1,
                    button: b1,
                },
                Binding::Controller {
                    device: d2,
                    instance: i2,
                    button: b2,
                },
            ) => b1 == b2 && (i1 == i2 || d1 == d2),
            (
                Binding::Hat {
                    device: d1,
                    instance: i1,
                    hat: h1,
                    direction: r1,
                },
                Binding::Hat {
                    device: d2,
                    instance: i2,
                    hat: h2,
                    direction: r2,
                },
            ) => h1 == h2 && r1 == r2 && (i1 == i2 || d1 == d2),
            (a, b) => a == b,
        }
    }

    fn is_escape(&self) -> bool {
        matches!(self, Binding::Key { key } if key == "Escape")
    }

    /// Inputs ignored during capture: left click (it is used to start the capture).
    fn ignored_in_capture(&self) -> bool {
        matches!(self, Binding::Mouse { button: 1 })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureResult {
    Bound(Binding),
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PttEvent {
    Pressed(Instant),
    Released(Instant),
}

/// Edge detection and capture/PTT logic, independent of the sources.
#[derive(Default)]
pub struct EdgeTracker {
    held: HashSet<Binding>,
}

impl EdgeTracker {
    /// Updates the state; returns (presses, releases).
    pub fn update(&mut self, now: &HashSet<Binding>) -> (Vec<Binding>, Vec<Binding>) {
        let pressed = now.difference(&self.held).cloned().collect();
        let released = self.held.difference(now).cloned().collect();
        self.held = now.clone();
        (pressed, released)
    }

    pub fn is_held(&self, b: &Binding) -> bool {
        self.held.iter().any(|h| h.matches(b))
    }
}

struct Shared {
    ptt: Option<Binding>,
    capture: Option<Sender<CaptureResult>>,
    devices: Vec<String>,
}

pub struct InputService {
    shared: Arc<Mutex<Shared>>,
    ptt_rx: Receiver<PttEvent>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

const POLL: Duration = Duration::from_millis(5);

impl InputService {
    /// Starts reading with the platform sources.
    pub fn spawn() -> Self {
        Self::spawn_with(backend::platform_backends)
    }

    /// Variant with provided sources (tests).
    pub fn spawn_with<F>(make_backends: F) -> Self
    where
        F: FnOnce() -> Vec<Box<dyn Backend>> + Send + 'static,
    {
        let shared = Arc::new(Mutex::new(Shared {
            ptt: None,
            capture: None,
            devices: vec![],
        }));
        let (ptt_tx, ptt_rx) = unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (shared, stop) = (shared.clone(), stop.clone());
            std::thread::Builder::new()
                .name("input".into())
                .spawn(move || run(make_backends(), shared, ptt_tx, stop))
                .expect("failed to start the input thread")
        };
        InputService {
            shared,
            ptt_rx,
            stop,
            thread: Some(thread),
        }
    }

    /// Waits for the next input pressed on any device.
    /// Escape cancels. A new capture replaces the previous one.
    pub fn capture(&self) -> Receiver<CaptureResult> {
        let (tx, rx) = bounded(1);
        self.shared.lock().unwrap().capture = Some(tx);
        rx
    }

    pub fn cancel_capture(&self) {
        if let Some(tx) = self.shared.lock().unwrap().capture.take() {
            let _ = tx.send(CaptureResult::Cancelled);
        }
    }

    pub fn set_ptt(&self, binding: Option<Binding>) {
        self.shared.lock().unwrap().ptt = binding;
    }

    pub fn ptt(&self) -> Option<Binding> {
        self.shared.lock().unwrap().ptt.clone()
    }

    pub fn ptt_events(&self) -> &Receiver<PttEvent> {
        &self.ptt_rx
    }

    /// Detected game devices.
    pub fn devices(&self) -> Vec<String> {
        self.shared.lock().unwrap().devices.clone()
    }
}

impl Drop for InputService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(
    mut backends: Vec<Box<dyn Backend>>,
    shared: Arc<Mutex<Shared>>,
    ptt_tx: Sender<PttEvent>,
    stop: Arc<AtomicBool>,
) {
    let mut edges = EdgeTracker::default();
    let mut ptt_down = false;
    let mut last_devices = Instant::now() - Duration::from_secs(10);
    let mut now_set = HashSet::new();
    while !stop.load(Ordering::Relaxed) {
        now_set.clear();
        for b in backends.iter_mut() {
            b.poll(&mut now_set);
        }
        let (pressed, _released) = edges.update(&now_set);

        let mut s = shared.lock().unwrap();
        if last_devices.elapsed() > Duration::from_secs(2) {
            last_devices = Instant::now();
            s.devices = backends.iter().flat_map(|b| b.devices()).collect();
        }
        if s.capture.is_some()
            && let Some(first) = pressed.iter().find(|b| !b.ignored_in_capture())
        {
            let result = if first.is_escape() {
                CaptureResult::Cancelled
            } else {
                CaptureResult::Bound(first.clone())
            };
            if let Some(tx) = s.capture.take() {
                let _ = tx.send(result);
            }
        }
        let down = s.ptt.as_ref().is_some_and(|p| edges.is_held(p));
        drop(s);
        if down != ptt_down {
            ptt_down = down;
            let t = Instant::now();
            let _ = ptt_tx.send(if down {
                PttEvent::Pressed(t)
            } else {
                PttEvent::Released(t)
            });
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Simulated source: the current state is driven by the test.
    struct Fake(Arc<Mutex<HashSet<Binding>>>);

    impl Backend for Fake {
        fn poll(&mut self, out: &mut HashSet<Binding>) {
            out.extend(self.0.lock().unwrap().iter().cloned());
        }
        fn devices(&self) -> Vec<String> {
            vec!["Test wheel".into()]
        }
    }

    fn key(k: &str) -> Binding {
        Binding::Key { key: k.into() }
    }

    fn wheel(button: u16, instance: &str) -> Binding {
        Binding::Controller {
            device: "Test wheel".into(),
            instance: instance.into(),
            button,
        }
    }

    fn service() -> (InputService, Arc<Mutex<HashSet<Binding>>>) {
        let state: Arc<Mutex<HashSet<Binding>>> = Arc::default();
        let s = state.clone();
        let svc = InputService::spawn_with(move || vec![Box::new(Fake(s)) as Box<dyn Backend>]);
        (svc, state)
    }

    fn set(state: &Arc<Mutex<HashSet<Binding>>>, items: &[Binding]) {
        *state.lock().unwrap() = items.iter().cloned().collect();
        std::thread::sleep(Duration::from_millis(30));
    }

    #[test]
    fn capture_takes_first_new_press_and_ignores_held_keys() {
        let (svc, state) = service();
        set(&state, &[key("LShift")]); // already held before the capture
        let rx = svc.capture();
        set(&state, &[key("LShift"), Binding::Mouse { button: 1 }]); // left click ignored
        assert!(rx.try_recv().is_err());
        set(&state, &[key("LShift"), wheel(11, "g1")]);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            CaptureResult::Bound(wheel(11, "g1"))
        );
        assert_eq!(svc.devices(), vec!["Test wheel"]);
    }

    #[test]
    fn escape_cancels_capture() {
        let (svc, state) = service();
        let rx = svc.capture();
        set(&state, &[key("Escape")]);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            CaptureResult::Cancelled
        );
    }

    #[test]
    fn ptt_edges_follow_the_bound_button() {
        let (svc, state) = service();
        svc.set_ptt(Some(wheel(3, "g1")));
        set(&state, &[wheel(4, "g1")]);
        assert!(svc.ptt_events().try_recv().is_err());
        // Different USB port: different instance, same product and same button.
        set(&state, &[wheel(3, "g2")]);
        assert!(matches!(
            svc.ptt_events()
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            PttEvent::Pressed(_)
        ));
        set(&state, &[]);
        assert!(matches!(
            svc.ptt_events()
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            PttEvent::Released(_)
        ));
    }

    #[test]
    fn bindings_serialize_readably() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct S {
            ptt: Binding,
        }
        let s = S {
            ptt: Binding::Hat {
                device: "Button box".into(),
                instance: "{abc}".into(),
                hat: 0,
                direction: 2,
            },
        };
        let text = toml::to_string(&s).unwrap();
        assert!(text.contains("type = \"hat\""), "{text}");
        assert_eq!(toml::from_str::<S>(&text).unwrap(), s);
        assert_eq!(s.ptt.label(), "Button box: hat 1 down");
        assert_eq!(wheel(0, "x").label(), "Test wheel: button 1");
    }
}
