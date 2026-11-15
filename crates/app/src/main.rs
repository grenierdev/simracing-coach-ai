//! Point d'entrée : choisit la source de télémétrie et lance l'interface.
//!
//! ```text
//! simcoach                         télémétrie live iRacing (Windows)
//! simcoach --replay session.ibt    rejoue un .ibt comme s'il était live
//!          [--speed 2.0] [--loop]
//! simcoach --synthetic             rejoue une session synthétique générée en mémoire
//! simcoach --write-synthetic f.ibt écrit la session synthétique dans un fichier
//! ```

use std::path::PathBuf;

use anyhow::{Context, bail};
use telemetry::synthetic::SyntheticTrack;
use telemetry::{
    IbtFile, LiveSource, ReplaySource, TelemetryHub, TelemetryHubConfig, TelemetrySource,
};

#[derive(Debug, Default)]
struct Args {
    replay: Option<PathBuf>,
    synthetic: bool,
    write_synthetic: Option<PathBuf>,
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
            "--write-synthetic" => {
                args.write_synthetic =
                    Some(it.next().context("--write-synthetic <fichier>")?.into())
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
                    "simcoach [--replay fichier.ibt [--speed X] [--loop]] [--synthetic] [--write-synthetic fichier.ibt]"
                );
                std::process::exit(0);
            }
            other => bail!("argument inconnu : {other}"),
        }
    }
    Ok(args)
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = parse_args()?;

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

    ui::run("Coach simracing", move || {
        TelemetryHub::spawn(source, TelemetryHubConfig::default())
    });
    Ok(())
}
