//! Captures the next input from any device, then tracks push-to-talk on that
//! input for 10 s.
//!
//! `cargo run -p input --example capture`

use std::time::{Duration, Instant};

use input::{CaptureResult, InputService, PttEvent};

fn main() {
    let svc = InputService::spawn();
    std::thread::sleep(Duration::from_millis(300));
    println!("Devices: {:?}", svc.devices());
    println!("Press a key or a button… (Escape to cancel)");
    match svc.capture().recv() {
        Ok(CaptureResult::Bound(b)) => {
            println!("Bound: {}  ({b:?})", b.label());
            svc.set_ptt(Some(b));
            let end = Instant::now() + Duration::from_secs(10);
            while Instant::now() < end {
                if let Ok(e) = svc.ptt_events().recv_timeout(Duration::from_millis(100)) {
                    match e {
                        PttEvent::Pressed(_) => println!("PTT pressed"),
                        PttEvent::Released(_) => println!("PTT released"),
                    }
                }
            }
        }
        Ok(CaptureResult::Cancelled) => println!("Cancelled"),
        Err(_) => println!("Service stopped"),
    }
}
