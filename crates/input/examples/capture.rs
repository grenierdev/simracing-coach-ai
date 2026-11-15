//! Capture la prochaine entrée de n'importe quel périphérique, puis suit le
//! push-to-talk sur cette entrée pendant 10 s.
//!
//! `cargo run -p input --example capture`

use std::time::{Duration, Instant};

use input::{CaptureResult, InputService, PttEvent};

fn main() {
    let svc = InputService::spawn();
    std::thread::sleep(Duration::from_millis(300));
    println!("Périphériques : {:?}", svc.devices());
    println!("Appuie sur une touche ou un bouton… (Échap pour annuler)");
    match svc.capture().recv() {
        Ok(CaptureResult::Bound(b)) => {
            println!("Lié : {}  ({b:?})", b.label());
            svc.set_ptt(Some(b));
            let end = Instant::now() + Duration::from_secs(10);
            while Instant::now() < end {
                if let Ok(e) = svc.ptt_events().recv_timeout(Duration::from_millis(100)) {
                    match e {
                        PttEvent::Pressed(_) => println!("PTT appuyé"),
                        PttEvent::Released(_) => println!("PTT relâché"),
                    }
                }
            }
        }
        Ok(CaptureResult::Cancelled) => println!("Annulé"),
        Err(_) => println!("Service arrêté"),
    }
}
