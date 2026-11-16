//! Probe: launches the ACP adapter, opens a session and, with `--prompt`, sends
//! a message and prints the latency.
//!
//! ```sh
//! cargo run -p coach --example probe -- [--model haiku] [--prompt "Hello"] [-- adapter command…]
//! ```

use std::time::{Duration, Instant};

use coach::{AgentConfig, AgentStatus, CoachAgent, CoachEvent};

fn main() {
    let mut args = std::env::args().skip(1);
    let mut model = None;
    let mut prompt = None;
    let mut command = vec![];
    while let Some(a) = args.next() {
        match a.as_str() {
            "--model" => model = args.next(),
            "--prompt" => prompt = args.next(),
            "--" => command = args.by_ref().collect(),
            _ => {}
        }
    }
    if command.is_empty() {
        command = AgentConfig::default_command();
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let cfg = AgentConfig {
        label: "probe".into(),
        command,
        model,
        disable_thinking: true,
        env: vec![],
        cwd: std::env::current_dir().unwrap(),
        system_prompt: coach::DEFAULT_SYSTEM_PROMPT.into(),
        mcp: None,
    };
    let t0 = Instant::now();
    let agent = CoachAgent::spawn(cfg, rt.handle());
    let mut prompted = false;
    let mut turn_started = Instant::now();
    loop {
        let Ok(e) = agent.events().recv_timeout(Duration::from_secs(120)) else {
            println!("timed out");
            return;
        };
        match &e {
            CoachEvent::Status(AgentStatus::Ready { agent: name }) if !prompted => {
                println!(
                    "session ready in {:.0} ms (agent: {name})",
                    t0.elapsed().as_secs_f64() * 1e3
                );
                match &prompt {
                    Some(p) => {
                        prompted = true;
                        turn_started = Instant::now();
                        agent.prompt(p.clone());
                    }
                    None => return,
                }
            }
            CoachEvent::Status(AgentStatus::Failed(err)) => {
                println!("failed: {err}");
                return;
            }
            CoachEvent::Chunk { text, .. } => print!("{text}"),
            CoachEvent::Error(err) => println!("\nerror: {err}"),
            CoachEvent::TurnEnd {
                latency,
                stop_reason,
                ..
            } => {
                println!(
                    "\nend ({stop_reason}): first chunk {:?} ms, total {:.0} ms (measured here: {:.0} ms)",
                    latency.first_chunk_ms.map(|x| x.round()),
                    latency.total_ms,
                    turn_started.elapsed().as_secs_f64() * 1e3
                );
                return;
            }
            _ => {}
        }
    }
}
