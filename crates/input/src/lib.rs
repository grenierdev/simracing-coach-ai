//! Périphériques d'entrée lus en arrière-plan, pendant qu'iRacing a le focus.
//!
//! Sources :
//! - clavier et souris : état global (`device_query` : `GetAsyncKeyState` sous
//!   Windows, X11 sous Linux) ;
//! - volants, boîtiers à boutons, joysticks, manettes : DirectInput en mode
//!   arrière-plan non exclusif sous Windows (l'API qu'utilise iRacing ; Windows
//!   Gaming Input exige le focus), `gilrs` (evdev) sous Linux.
//!
//! Le service interroge toutes les sources à 200 Hz et calcule les fronts
//! (appui, relâchement). Deux usages : la capture d'une liaison (« appuie sur
//! une touche, Échap pour annuler ») et le suivi du bouton de push-to-talk.

mod backend;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use serde::{Deserialize, Serialize};

pub use backend::Backend;

/// Une entrée identifiable : touche, bouton de souris ou bouton de périphérique.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Binding {
    /// Touche du clavier (nom `device_query`, ex. « F13 », « Space », « LControl »).
    Key { key: String },
    /// Bouton de souris (1 = gauche, 2 = droit, 3 = milieu, 4 et 5 = latéraux).
    Mouse { button: u8 },
    /// Bouton d'un périphérique de jeu.
    Controller {
        /// Nom du produit, affiché et utilisé si l'identifiant ne correspond plus.
        device: String,
        /// Identifiant de l'instance (GUID DirectInput ou UUID gilrs).
        instance: String,
        button: u16,
    },
    /// Direction d'un chapeau (POV) : 0 haut, 1 droite, 2 bas, 3 gauche.
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
            Binding::Key { key } => format!("Clavier : {key}"),
            Binding::Mouse { button } => format!("Souris : bouton {button}"),
            Binding::Controller { device, button, .. } => {
                format!("{device} : bouton {}", button + 1)
            }
            Binding::Hat {
                device,
                hat,
                direction,
                ..
            } => {
                let d = ["haut", "droite", "bas", "gauche"]
                    .get(*direction as usize)
                    .copied()
                    .unwrap_or("?");
                format!("{device} : chapeau {} {d}", hat + 1)
            }
        }
    }

    /// Même entrée physique. Pour un périphérique, l'identifiant d'instance prime ;
    /// s'il a changé (autre port USB), le nom du produit suffit.
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

    /// Entrées ignorées en capture : le clic gauche (il sert à lancer la capture).
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

/// Calcul des fronts et logique de capture/PTT, indépendant des sources.
#[derive(Default)]
pub struct EdgeTracker {
    held: HashSet<Binding>,
}

impl EdgeTracker {
    /// Met à jour l'état ; retourne (appuis, relâchements).
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
    /// Démarre la lecture avec les sources de la plateforme.
    pub fn spawn() -> Self {
        Self::spawn_with(backend::platform_backends)
    }

    /// Variante avec des sources fournies (tests).
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
                .expect("démarrage du thread des entrées")
        };
        InputService {
            shared,
            ptt_rx,
            stop,
            thread: Some(thread),
        }
    }

    /// Attend la prochaine entrée appuyée sur n'importe quel périphérique.
    /// Échap annule. Une nouvelle capture remplace la précédente.
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

    /// Périphériques de jeu détectés.
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

    /// Source simulée : l'état courant est piloté par le test.
    struct Fake(Arc<Mutex<HashSet<Binding>>>);

    impl Backend for Fake {
        fn poll(&mut self, out: &mut HashSet<Binding>) {
            out.extend(self.0.lock().unwrap().iter().cloned());
        }
        fn devices(&self) -> Vec<String> {
            vec!["Volant de test".into()]
        }
    }

    fn key(k: &str) -> Binding {
        Binding::Key { key: k.into() }
    }

    fn wheel(button: u16, instance: &str) -> Binding {
        Binding::Controller {
            device: "Volant de test".into(),
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
        set(&state, &[key("LShift")]); // déjà enfoncée avant la capture
        let rx = svc.capture();
        set(&state, &[key("LShift"), Binding::Mouse { button: 1 }]); // clic gauche ignoré
        assert!(rx.try_recv().is_err());
        set(&state, &[key("LShift"), wheel(11, "g1")]);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            CaptureResult::Bound(wheel(11, "g1"))
        );
        assert_eq!(svc.devices(), vec!["Volant de test"]);
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
        // Autre port USB : instance différente, même produit et même bouton.
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
                device: "Boîtier".into(),
                instance: "{abc}".into(),
                hat: 0,
                direction: 2,
            },
        };
        let text = toml::to_string(&s).unwrap();
        assert!(text.contains("type = \"hat\""), "{text}");
        assert_eq!(toml::from_str::<S>(&text).unwrap(), s);
        assert_eq!(s.ptt.label(), "Boîtier : chapeau 1 bas");
        assert_eq!(wheel(0, "x").label(), "Volant de test : bouton 1");
    }
}
