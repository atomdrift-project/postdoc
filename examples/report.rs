//! Produce one real result, for reviewing the wire shape against real output.
//!
//! ```sh
//! cargo run --profile quick --example report -- NEW [OLD]
//! ```
//!
//! Prints the version 8 body postdoc would upload for `NEW`, without the
//! worker name. `OLD`, when given, is the earlier release it is compared
//! against — what hopper would name as the baseline — and isomer's opinions
//! join scan's as the `drift_heuristic` and `drift_llm` jurors. Without it there are
//! no drift jurors, which is the ordinary case for an artifact the corpus has
//! no predecessor for. `POSTDOC_BASELINE_PURL` and `POSTDOC_BASELINE_VERSION`
//! stand in for the coordinates hopper would supply with the claim.
//! `POSTDOC_SCANNERS` (default `none`) adds third-party scanners' opinions
//! under `scanners`, as the daemons' `--scanners` does, found where the
//! daemons find them or at `POSTDOC_CLAMAV`, `POSTDOC_MALCONTENT` and
//! `POSTDOC_GUARDDOG`.
//!
//! An example rather than a test: it runs a real analysis against the model
//! and trait bundles, and postdoc's test suite stays runnable without either.

use std::error::Error;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use postdoc::judge::{Analysis, judge};
use postdoc::result::Baseline;
use postdoc::scanners::{Input, Scanners, Settings, parse_names};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (new, old) = match args.as_slice() {
        [new] => (PathBuf::from(new), None),
        [new, old] => (PathBuf::from(new), Some(PathBuf::from(old))),
        _ => {
            eprintln!("usage: report NEW [OLD]");
            std::process::exit(2);
        }
    };

    let scanners = Scanners::start(Settings {
        names: parse_names(&std::env::var("POSTDOC_SCANNERS").unwrap_or_default())?,
        clamav: std::env::var_os("POSTDOC_CLAMAV").map(PathBuf::from),
        malcontent: std::env::var_os("POSTDOC_MALCONTENT").map(PathBuf::from),
        guarddog: std::env::var_os("POSTDOC_GUARDDOG").map(PathBuf::from),
    })?;
    let analyzer = scan::Analyzer::load(scan::models_repo::ensure_model_dir()?)?;
    let scanned = analyzer.scan_file(&new, &filename(&new))?;

    // What hopper would have handed over with the claim. Here the predecessor
    // is scanned too, so the baseline carries a real digest and a real verdict
    // of its own rather than placeholders.
    let baseline = old
        .as_deref()
        .map(|old| -> Result<Baseline, Box<dyn Error>> {
            let before = analyzer.scan_file(old, &filename(old))?;
            Ok(Baseline {
                sha256: before.sha256,
                purl: std::env::var("POSTDOC_BASELINE_PURL").ok(),
                version: std::env::var("POSTDOC_BASELINE_VERSION").ok(),
                label: Some("good".to_owned()),
                fires_at: postdoc::engine::level(before.level),
            })
        })
        .transpose()?;

    let judged = old
        .as_deref()
        .map(|old| {
            isomer::judgement::judge(
                old,
                &new,
                &isomer::options::Options {
                    offline: true,
                    ..isomer::options::Options::default()
                },
            )
        })
        .transpose()?;

    let mut result = judge(Analysis {
        scan: scanned,
        isomer: judged,
        baseline,
        model: analyzer.model(),
    })?;
    scanners.run(&mut result, Input::Path(&new));
    let body = result
        .full_body(false)
        .ok_or("the result could not be written as a version 8 body")?;
    std::io::stdout().lock().write_all(&body)?;
    Ok(())
}

fn filename(path: &Path) -> String {
    path.file_name().map_or_else(
        || "artifact".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    )
}
