//! Point d'entrée : choisit la source de télémétrie et lance l'interface.
//!
//! ```text
//! simcoach                         télémétrie live iRacing (Windows)
//! simcoach --replay session.ibt    rejoue un .ibt comme s'il était live
//!          [--speed 2.0] [--loop]
//! simcoach --synthetic             rejoue une session synthétique générée en mémoire
//! simcoach ... --reference ref.csv  tour de référence (CSV Garage61 ou .ibt)
//! simcoach --write-synthetic f.ibt écrit la session synthétique dans un fichier
//! simcoach --write-synthetic-reference <dossier>
//!                                  écrit un tour de référence synthétique (CSV Garage61)
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
    reference: Option<PathBuf>,
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
                    "simcoach [--replay fichier.ibt [--speed X] [--loop]] [--synthetic] [--reference fichier.csv|.ibt] [--write-synthetic fichier.ibt] [--write-synthetic-reference dossier]"
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

    if let Some(dir) = &args.write_synthetic_reference {
        // Tour « pro » au rythme limite, au format CSV Garage61, sans YawRate ni
        // LatAccel (estimés à l'import depuis la trajectoire).
        let samples = SyntheticTrack::demo().simulate_lap(1.0, 0.0, 0.0);
        let t = samples.len() as f64 / 60.0;
        let name = format!(
            "Garage 61 - Pilote Pro - Voiture synthétique - Circuit synthétique - {:02}.{:02}.{:03} - demo.csv",
            (t / 60.0) as u32,
            (t % 60.0) as u32,
            ((t.fract()) * 1000.0).round() as u32
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
        let path = dir.join(name);
        std::fs::write(&path, reference::write_garage61_csv(&samples, &columns))
            .with_context(|| format!("écriture de {}", path.display()))?;
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

    ui::run("Coach simracing", args.reference.clone(), move || {
        TelemetryHub::spawn(source, TelemetryHubConfig::default())
    });
    Ok(())
}
