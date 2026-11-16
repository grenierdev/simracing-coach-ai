//! Downloads the voice models: the Piper voices (English and French, or the
//! one given with `--language`), plus Parakeet with `--stt`.
//!
//! `cargo run -p voice --example download_models [dir] [--language en|fr] [--stt]`

use voice::Language;
use voice::models::{PARAKEET_V3_INT8, ensure, models_dir};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut dir = None;
    let mut languages = Language::ALL.to_vec();
    let mut stt = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--stt" => stt = true,
            "--language" => {
                let l = it.next().and_then(|v| Language::parse(v));
                languages = vec![l.expect("--language en|fr")];
            }
            other => dir = Some(other.into()),
        }
    }
    let dir = dir.unwrap_or_else(models_dir);
    let mut specs: Vec<_> = languages.iter().map(|l| *l.tts_model()).collect();
    if stt {
        specs.push(PARAKEET_V3_INT8);
    }
    for spec in specs {
        let mut last = 0;
        let path = ensure(&spec, &dir, |done, total| {
            let pct = (done * 100).checked_div(total).unwrap_or(0);
            if pct >= last + 10 {
                last = pct;
                println!("{}: {pct}%", spec.dir_name());
            }
        })
        .unwrap_or_else(|e| panic!("{}: {e}", spec.repo));
        println!("{} → {}", spec.repo, path.display());
    }
}
