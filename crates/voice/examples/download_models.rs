//! Télécharge les modèles vocaux (Piper siwis, et Parakeet avec `--stt`).
//!
//! `cargo run -p voice --example download_models [dossier] [--stt]`

use voice::models::{PARAKEET_V3_INT8, PIPER_FR_SIWIS, ensure, models_dir};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(Into::into)
        .unwrap_or_else(models_dir);
    let mut specs = vec![PIPER_FR_SIWIS];
    if args.iter().any(|a| a == "--stt") {
        specs.push(PARAKEET_V3_INT8);
    }
    for spec in specs {
        let mut last = 0;
        let path = ensure(&spec, &dir, |done, total| {
            let pct = (done * 100).checked_div(total).unwrap_or(0);
            if pct >= last + 10 {
                last = pct;
                println!("{} : {pct} %", spec.dir_name());
            }
        })
        .unwrap_or_else(|e| panic!("{} : {e}", spec.repo));
        println!("{} → {}", spec.repo, path.display());
    }
}
