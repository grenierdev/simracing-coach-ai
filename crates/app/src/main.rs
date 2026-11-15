//! Point d'entrée : choisit la source de télémétrie et lance l'interface.
//!
//! ```text
//! simcoach                         télémétrie live iRacing (Windows)
//! simcoach --replay session.ibt    rejoue un .ibt comme s'il était live
//!          [--speed 2.0] [--loop]
//! simcoach --synthetic             rejoue une session synthétique générée en mémoire
//! simcoach ... --reference ref.csv  tour de référence (CSV Garage61 ou .ibt)
//! simcoach ... --plan plan.toml     plan de consignes
//! simcoach ... --no-voice           consignes affichées sans être dites
//! simcoach ... --no-agent           sans Claude (pas d'adaptateur ACP)
//! simcoach ... --model sonnet       modèle de la conversation (défaut : haiku)
//! simcoach --write-synthetic f.ibt écrit la session synthétique dans un fichier
//! simcoach --write-synthetic-reference <dossier>
//!                                  écrit un tour de référence synthétique (CSV Garage61)
//! ```

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
    synthetic: bool,
    write_synthetic: Option<PathBuf>,
    write_synthetic_reference: Option<PathBuf>,
    speed: f64,
    looping: bool,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut args = Args {
        speed: 1.0,
        ..Default::default()
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--replay" => args.replay = Some(it.next().context("--replay <fichier.ibt>")?.into()),
            "--synthetic" => args.synthetic = true,
            "--plan" => args.plan = Some(it.next().context("--plan <fichier.toml>")?.into()),
            "--no-voice" => args.no_voice = true,
            "--no-agent" => args.no_agent = true,
            "--model" => {
                args.model = Some(
                    it.next()
                        .context("--model <haiku|sonnet|opus|identifiant>")?,
                )
            }
            "--reference" => {
                args.reference = Some(
                    it.next()
                        .context("--reference <fichier .csv ou .ibt>")?
                        .into(),
                )
            }
            "--write-synthetic" => {
                args.write_synthetic =
                    Some(it.next().context("--write-synthetic <fichier>")?.into())
            }
            "--write-synthetic-reference" => {
                args.write_synthetic_reference = Some(
                    it.next()
                        .context("--write-synthetic-reference <dossier>")?
                        .into(),
                )
            }
            "--speed" => {
                args.speed = it
                    .next()
                    .context("--speed <facteur>")?
                    .parse()
                    .context("--speed attend un nombre")?
            }
            "--loop" => args.looping = true,
            "-h" | "--help" => {
                println!(
                    "simcoach [--replay fichier.ibt [--speed X] [--loop]] [--synthetic] [--reference fichier.csv|.ibt] [--plan plan.toml] [--no-voice] [--no-agent] [--model haiku] [--write-synthetic fichier.ibt] [--write-synthetic-reference dossier]"
                );
                std::process::exit(0);
            }
            other => bail!("argument inconnu : {other}"),
        }
    }
    Ok(args)
}

fn main() -> anyhow::Result<()> {
    // Claude passe par l'abonnement du CLI : une clé API dans l'environnement
    // serait utilisée (et facturée) à sa place par l'adaptateur. Retirée avant
    // tout démarrage de thread.
    if std::env::var_os("ANTHROPIC_API_KEY").is_some() {
        // SAFETY : aucun autre thread n'existe encore.
        unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };
        eprintln!("ANTHROPIC_API_KEY ignorée : le coach utilise l'abonnement du CLI Claude.");
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
        std::fs::write(&path, csv).with_context(|| format!("écriture de {}", path.display()))?;
        println!("Référence synthétique écrite dans {}", path.display());
        return Ok(());
    }

    if let Some(path) = &args.write_synthetic {
        SyntheticTrack::demo()
            .session_ibt(10, 1)
            .write_to(path)
            .with_context(|| format!("écriture de {}", path.display()))?;
        println!("Session synthétique écrite dans {}", path.display());
        return Ok(());
    }

    let source: Box<dyn TelemetrySource> = if let Some(path) = &args.replay {
        let file = IbtFile::open(path).with_context(|| format!("lecture de {}", path.display()))?;
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Box::new(ReplaySource::new(file, label, args.speed, args.looping))
    } else if args.synthetic {
        let bytes = SyntheticTrack::demo().session_ibt(10, 1).to_bytes();
        let file = IbtFile::from_bytes(bytes)?;
        Box::new(ReplaySource::new(file, "synthétique", args.speed, true))
    } else {
        if !cfg!(windows) {
            bail!(
                "la télémétrie live n'existe que sous Windows : utiliser --replay ou --synthetic"
            );
        }
        Box::new(LiveSource::new())
    };

    // Référence et plan : ceux de la ligne de commande ; en mode synthétique,
    // la référence et le plan de démonstration par défaut.
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
        let demo = PathBuf::from("plans/synthetique.toml");
        (args.synthetic && demo.exists()).then_some(demo)
    });
    let no_voice = args.no_voice;
    let agent_options = agent::AgentOptions {
        enabled: !args.no_agent,
        model: args.model.clone().unwrap_or_else(|| "haiku".into()),
    };

    ui::run(
        "Coach simracing",
        ui::StartOptions { reference, plan },
        move || {
            let hub = TelemetryHub::spawn(source, TelemetryHubConfig::default());
            let speaker = speech::start_speaker(no_voice);
            let triggers = std::sync::Arc::new(triggers::TriggerRuntime::spawn(
                hub.subscribe_samples(),
                Box::new(speech::SpeakerOutput(speaker.clone())),
            ));
            let agent = agent::start(hub.snapshot_handle(), triggers.clone(), &agent_options);
            ui::Services {
                hub,
                triggers,
                speaker,
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

/// Tour « pro » synthétique au rythme limite, au format CSV Garage61, sans
/// YawRate ni LatAccel (estimés à l'import depuis la trajectoire).
fn synthetic_reference_csv() -> (String, String) {
    let samples = SyntheticTrack::demo().simulate_lap(1.0, 0.0, 0.0);
    let t = samples.len() as f64 / 60.0;
    let name = format!(
        "Garage 61 - Pilote Pro - Voiture synthétique - Circuit synthétique - {:02}.{:02}.{:03} - demo.csv",
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
