//! Entry point: picks the telemetry source and starts the UI.
//!
//! ```text
//! simcoach                         live iRacing telemetry (Windows)
//! simcoach --replay session.ibt    replays an .ibt as if it were live
//!          [--speed 2.0] [--loop]
//! simcoach --synthetic             replays a synthetic session generated in memory
//! simcoach ... --reference ref.csv  reference lap (Garage61 CSV or .ibt)
//! simcoach ... --plan plan.toml     cue plan
//! simcoach ... --no-voice           cues shown but not spoken
//! simcoach ... --no-agent           without Claude (no ACP adapter)
//! simcoach ... --model sonnet       conversation model (default: haiku)
//! simcoach ... --analysis-model opus  planning model (default: sonnet)
//! simcoach ... --no-planning        without analysis session
//! simcoach ... --language fr        voice language, en or fr (default: settings)
//! simcoach --write-synthetic f.ibt writes the synthetic session to a file
//! simcoach --write-synthetic-reference <folder>
//!                                  writes a synthetic reference lap (Garage61 CSV)
//! ```
//!
//! Release builds are Windows GUI programs: no console window opens with
//! them. Started from a terminal, they still write to it.

// Debug builds keep their console for development.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent;
mod speech;

use std::path::PathBuf;

use anyhow::{Context, bail};
use telemetry::synthetic::SyntheticTrack;
use telemetry::{
    IbtFile, LiveSource, ReplaySource, TelemetryHub, TelemetryHubConfig, TelemetrySource,
};

#[derive(Debug, Default)]
struct Args {
    replay: Option<PathBuf>,
    reference: Option<PathBuf>,
    plan: Option<PathBuf>,
    no_voice: bool,
    no_agent: bool,
    model: Option<String>,
    analysis_model: Option<String>,
    no_planning: bool,
    synthetic: bool,
    write_synthetic: Option<PathBuf>,
    write_synthetic_reference: Option<PathBuf>,
    speed: f64,
    looping: bool,
    language: Option<voice::Language>,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut args = Args {
        speed: 1.0,
        ..Default::default()
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--replay" => args.replay = Some(it.next().context("--replay <file.ibt>")?.into()),
            "--synthetic" => args.synthetic = true,
            "--plan" => args.plan = Some(it.next().context("--plan <file.toml>")?.into()),
            "--no-voice" => args.no_voice = true,
            "--no-agent" => args.no_agent = true,
            "--no-planning" => args.no_planning = true,
            "--analysis-model" => {
                args.analysis_model = Some(it.next().context("--analysis-model <sonnet|opus|id>")?)
            }
            "--model" => args.model = Some(it.next().context("--model <haiku|sonnet|opus|id>")?),
            "--reference" => {
                args.reference = Some(it.next().context("--reference <.csv or .ibt file>")?.into())
            }
            "--write-synthetic" => {
                args.write_synthetic = Some(it.next().context("--write-synthetic <file>")?.into())
            }
            "--write-synthetic-reference" => {
                args.write_synthetic_reference = Some(
                    it.next()
                        .context("--write-synthetic-reference <folder>")?
                        .into(),
                )
            }
            "--speed" => {
                args.speed = it
                    .next()
                    .context("--speed <factor>")?
                    .parse()
                    .context("--speed expects a number")?
            }
            "--loop" => args.looping = true,
            "--language" => {
                let v = it.next().context("--language <en|fr>")?;
                args.language =
                    Some(voice::Language::parse(&v).with_context(|| {
                        format!("--language: unknown language \"{v}\" (en or fr)")
                    })?);
            }
            "-h" | "--help" => {
                println!(
                    "simcoach [--replay file.ibt [--speed X] [--loop]] [--synthetic] [--reference file.csv|.ibt] [--plan plan.toml] [--no-voice] [--no-agent] [--model haiku] [--analysis-model sonnet] [--no-planning] [--language en|fr] [--write-synthetic file.ibt] [--write-synthetic-reference folder]"
                );
                std::process::exit(0);
            }
            other => bail!("unknown argument: {other}"),
        }
    }
    Ok(args)
}

/// A GUI program has no console: when started from a terminal, write the
/// messages and `--help` to that terminal. Started from Explorer, or in a
/// debug build (which has its own console), the call just fails.
#[cfg(windows)]
fn attach_parent_console() {
    use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    // SAFETY: plain Win32 call with an integer argument.
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

fn main() -> anyhow::Result<()> {
    #[cfg(windows)]
    attach_parent_console();
    // Claude goes through the CLI subscription: an API key in the environment
    // would be used (and billed) instead by the adapter. Removed before any
    // thread starts.
    if std::env::var_os("ANTHROPIC_API_KEY").is_some() {
        // SAFETY: no other thread exists yet.
        unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };
        eprintln!("ANTHROPIC_API_KEY ignored: the coach uses the Claude CLI subscription.");
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = parse_args()?;

    if let Some(dir) = &args.write_synthetic_reference {
        let (name, csv) = synthetic_reference_csv();
        let path = dir.join(name);
        std::fs::write(&path, csv).with_context(|| format!("writing {}", path.display()))?;
        println!("Synthetic reference written to {}", path.display());
        return Ok(());
    }

    if let Some(path) = &args.write_synthetic {
        SyntheticTrack::demo()
            .session_ibt(10, 1)
            .write_to(path)
            .with_context(|| format!("writing {}", path.display()))?;
        println!("Synthetic session written to {}", path.display());
        return Ok(());
    }

    let source: Box<dyn TelemetrySource> = if let Some(path) = &args.replay {
        let file = IbtFile::open(path).with_context(|| format!("reading {}", path.display()))?;
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Box::new(ReplaySource::new(file, label, args.speed, args.looping))
    } else if args.synthetic {
        let bytes = SyntheticTrack::demo().session_ibt(10, 1).to_bytes();
        let file = IbtFile::from_bytes(bytes)?;
        Box::new(ReplaySource::new(file, "synthetic", args.speed, true))
    } else {
        if !cfg!(windows) {
            bail!("live telemetry is only available on Windows: use --replay or --synthetic");
        }
        Box::new(LiveSource::new())
    };

    // Reference and plan: those from the command line; in synthetic mode,
    // the demo reference and plan by default.
    let reference = match (&args.reference, args.synthetic) {
        (Some(p), _) => Some(ui::ReferenceSource::File(p.clone())),
        (None, true) => {
            let (name, csv) = synthetic_reference_csv();
            let r = reference::from_garage61_csv(csv.as_bytes(), &name)?;
            Some(ui::ReferenceSource::Preloaded(Box::new(r)))
        }
        (None, false) => None,
    };
    let plan = args.plan.clone().or_else(|| {
        let demo = PathBuf::from("plans/synthetic.toml");
        (args.synthetic && demo.exists()).then_some(demo)
    });
    let no_voice = args.no_voice;
    let settings = ui::settings::Settings::load();
    let language = args.language.unwrap_or(settings.language);
    tracing::info!("voice language: {}", language.name());
    let coaching = mcp::CoachingLink::default();
    let agent_options = agent::AgentOptions {
        enabled: !args.no_agent,
        model: args.model.clone().unwrap_or_else(|| "haiku".into()),
        analysis: !args.no_planning,
        analysis_model: args
            .analysis_model
            .clone()
            .unwrap_or_else(|| "sonnet".into()),
        language,
        coaching: coaching.clone(),
    };

    ui::run(
        "Simracing coach",
        ui::StartOptions { reference, plan },
        move || {
            let hub = TelemetryHub::spawn(source, TelemetryHubConfig::default());
            let mut notes = vec![];
            let (speaker, output) = speech::start_speaker(
                no_voice,
                language,
                ui::settings::DeviceChoice::id(&settings.output),
                &mut notes,
            );
            if let Some(o) = &output {
                o.set_volume(settings.volume.min(ui::settings::MAX_VOLUME) as f32 / 100.0);
            }
            let triggers = std::sync::Arc::new(triggers::TriggerRuntime::spawn(
                hub.subscribe_samples(),
                Box::new(speech::SpeakerOutput(speaker.clone())),
            ));
            let agent = agent::start(hub.snapshot_handle(), triggers.clone(), &agent_options);
            let planner = agent.analysis.clone().map(|analysis| {
                ui::planner::Planner::spawn(
                    ui::planner::PlannerDeps {
                        agent: analysis,
                        model: agent_options.analysis_model.clone(),
                        laps: hub.events().clone(),
                        samples: hub.subscribe_samples(),
                        triggers: triggers.clone(),
                        link: coaching.clone(),
                        language,
                    },
                    ui::planner::PlannerPolicy::default(),
                    settings.auto_planning,
                )
            });
            let input = std::sync::Arc::new(input::InputService::spawn());
            input.set_ptt(settings.ptt.clone());
            let recorder = speech::open_microphone(
                ui::settings::DeviceChoice::id(&settings.microphone),
                &mut notes,
            );
            for n in &notes {
                tracing::warn!("{n}");
            }
            let listener = std::sync::Arc::new(speech::start_listener());
            let conversation = ui::conversation::Conversation::spawn(
                ui::conversation::ConversationDeps {
                    agent: agent.agent.clone(),
                    speaker: speaker.clone(),
                    triggers: triggers.clone(),
                    snapshot: hub.snapshot_handle(),
                    input: Some(input.clone()),
                    recorder: Some(recorder.clone() as std::sync::Arc<dyn voice::Microphone>),
                    listener: Some(listener.clone()),
                    coaching: planner.is_some().then(|| coaching.clone()),
                    language,
                },
                settings.speak_replies,
            );
            ui::Services {
                hub,
                triggers,
                speaker,
                conversation,
                input: Some(input),
                listener: Some(listener),
                recorder: Some(recorder),
                output,
                notes,
                settings,
                language,
                planner,
                agent: agent.agent,
                mcp_url: agent.mcp_url,
                mcp_error: agent.mcp_error,
                tool_log: agent.tool_log,
                keep_alive: agent.keep_alive,
            }
        },
    );
    Ok(())
}

/// Synthetic "pro" lap at the limit, in Garage61 CSV format, without YawRate
/// or LatAccel (estimated from the trajectory on import).
fn synthetic_reference_csv() -> (String, String) {
    let samples = SyntheticTrack::demo().simulate_lap(1.0, 0.0, 0.0);
    let t = samples.len() as f64 / 60.0;
    let name = format!(
        "Garage 61 - Pro Driver - Synthetic Car - Synthetic Track - {:02}.{:02}.{:03} - demo.csv",
        (t / 60.0) as u32,
        (t % 60.0) as u32,
        (t.fract() * 1000.0).round() as u32
    );
    let columns = [
        "Speed",
        "LapDistPct",
        "Lat",
        "Lon",
        "Brake",
        "Throttle",
        "RPM",
        "SteeringWheelAngle",
        "Gear",
        "Clutch",
        "LongAccel",
        "PositionType",
    ];
    (name, reference::write_garage61_csv(&samples, &columns))
}
