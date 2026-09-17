//! Produce one real report, for reviewing the wire shape against real output.
//!
//! ```sh
//! cargo run --profile quick --example report -- NEW [OLD]
//! ```
//!
//! `POSTDOC_PURL`, `POSTDOC_BASELINE_PURL` and `POSTDOC_BASELINE_VERSION`
//! stand in for the coordinates hopper would supply with the claim.
//!
//! `NEW` is the artifact to judge. `OLD`, when given, is the earlier release
//! it is compared against — what hopper would name as the baseline. Without
//! it the `diff` and `diff_llm` judges are skipped, which is the ordinary
//! case for an artifact the corpus has no predecessor for.
//!
//! An example rather than a test: it runs a real analysis against the model
//! and trait bundles, and postdoc's test suite stays runnable without either.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::Instant;

use postdoc::judge::{Analysis, report};
use postdoc::report::{Engines, Skipped};
use postdoc::result::Baseline;

type Fallible = Result<(), Box<dyn Error>>;

fn main() -> Fallible {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (new, old) = match args.as_slice() {
        [new] => (PathBuf::from(new), None),
        [new, old] => (PathBuf::from(new), Some(PathBuf::from(old))),
        _ => {
            eprintln!("usage: report NEW [OLD]");
            std::process::exit(2);
        }
    };

    let started = Instant::now();
    let analyzer = scan::Analyzer::load(scan::models_repo::model_dir()?)?;
    let scanned = analyzer.scan_file(&new, &filename(&new))?;

    // What hopper would have handed over with the claim. Here the predecessor
    // is scanned too, so the baseline row carries a real digest and a real
    // verdict of its own rather than placeholders.
    let baseline = old
        .as_deref()
        .map(|old| -> Result<Baseline, Box<dyn Error>> {
            let before = analyzer.scan_file(old, &filename(old))?;
            Ok(Baseline {
                sha256: before.sha256.clone(),
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

    let engines = Engines {
        postdoc: postdoc::VERSION.to_owned(),
        scan: scan::engine::ENGINE_VERSION.to_owned(),
        cleave: cleave_version(),
        isomer: isomer::VERSION.to_owned(),
        traits: scanned
            .cleave
            .as_ref()
            .and_then(|report| report.traits_version.clone())
            .unwrap_or_else(|| "unknown".to_owned()),
        model: scanned.version.clone(),
    };

    let built = report(Analysis {
        scan: scanned,
        isomer: judged,
        baseline,
        purl: std::env::var("POSTDOC_PURL").ok(),
        // This example never configures one.
        llm_skipped: Skipped::NotConfigured,
        model: analyzer.model(),
        engines,
        analyzed_at: now_rfc3339(),
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    })?;

    println!("{}", serde_json::to_string_pretty(&built)?);
    Ok(())
}

fn filename(path: &Path) -> String {
    path.file_name().map_or_else(
        || "artifact".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// Close enough for an example; the worker stamps these from a real clock.
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = secs / 86_400;
    let (h, m, s) = ((secs % 86_400) / 3600, (secs % 3600) / 60, secs % 60);
    // Civil date from days since epoch (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mo <= 2);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Cleave does not export its own crate version, and the traits revision it
/// does export is already reported separately, so this names the bundle the
/// analysis actually ran against.
fn cleave_version() -> String {
    let info = cleave::version_info();
    format!(
        "{} traits, {} composites",
        info.trait_count, info.composite_count
    )
}
