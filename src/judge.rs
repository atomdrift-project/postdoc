//! Turning what the engines returned into a report.
//!
//! Each engine's result is unpacked into the judges that show its pieces, the
//! two finished outcomes are folded, and the verdict is written. Nothing here
//! grades: every severity is one an engine settled on, and the only choices
//! postdoc makes are which opinion to quote and which judges to credit.

use scan::engine::{ScanResult, synthesized_level};
use scan::model::Model;

use crate::combine::{self, Outcome};
use crate::engine::{classification, isomer_severity, level};
use crate::report::{Assessment, Engines, Finding, Judge, JudgeId, Severity, Skipped, Verdict};
use crate::result::{Baseline, Report, SCHEMA_VERSION};

/// Everything postdoc knows about one artifact.
///
/// The two engine results are taken by value: their reports are the largest
/// thing in a scan, and a report is where they are going.
#[derive(Debug)]
pub struct Analysis<'a> {
    /// What scan concluded.
    pub scan: ScanResult,
    /// What isomer concluded, when there was a baseline to compare against.
    pub isomer: Option<isomer::judgement::Judgement>,
    /// The release isomer compared against.
    pub baseline: Option<Baseline>,
    /// The coordinate the artifact was fetched as.
    pub purl: Option<String>,
    /// Why scan's interpreter did not run, when it did not. The caller knows
    /// whether it was configured; the result alone cannot tell.
    pub llm_skipped: Skipped,
    /// The loaded model, for placing a synthesized verdict inside its band.
    pub model: &'a Model,
    /// The builds behind this result.
    pub engines: Engines,
    /// RFC 3339, UTC.
    pub analyzed_at: String,
    /// Wall-clock for the whole analysis.
    pub duration_ms: u64,
}

/// Write the report for one analysis.
///
/// # Errors
///
/// Returns the serialization failure if an engine's own output cannot be
/// encoded. postdoc does not inspect that output, so this means the engine
/// produced something it could not itself serialize.
pub fn report(analysis: Analysis<'_>) -> Result<Report, serde_json::Error> {
    let Analysis {
        scan,
        isomer,
        baseline,
        purl,
        llm_skipped,
        model,
        engines,
        analyzed_at,
        duration_ms,
    } = analysis;

    // Read what the judges need before the result is consumed into the
    // envelope that carries their evidence.
    let sha256 = scan.sha256.clone();
    let model_decision = scan.model;
    let floor = scan.floor;
    let scan_outcome = Outcome {
        severity: scan.classification.into(),
        fires_at: level(scan.level),
    };
    let interpret_ms = scan.interpret_ms;
    let traits = scan
        .cleave
        .as_ref()
        .and_then(|report| report.traits_version.clone());
    let findings = scan
        .cleave
        .as_ref()
        .map(|report| scan::lookup::collect_hits(report, purl.as_deref()))
        .unwrap_or_default()
        .into_iter()
        .map(|hit| finding(hit, purl.as_deref()))
        .collect();
    let envelope = scan.into_envelope();

    // cleave: the trait floor's reading of what cleave found. The floor has no
    // way to say benign, so a silent floor is reported as benign here rather
    // than as an absence — cleave did look, and found nothing reaching its
    // bars.
    let cleave = Judge::Ok(Assessment {
        severity: floor.map_or(Severity::Benign, |f| f.class.into()),
        fires_at: floor.and_then(|f| level(f.level)),
        confidence: floor.map(|f| f.confidence),
        duration_ms: None,
        version: traits.unwrap_or_else(|| "unknown".to_owned()),
        raw: Some(raw(&envelope.raw)?),
    });

    // ml: the model alone, before the floor and before the interpreter.
    let ml = Judge::Ok(Assessment {
        severity: model_decision.class.into(),
        fires_at: level(model_decision.level),
        confidence: Some(model_decision.probability),
        duration_ms: None,
        version: engines.model.clone(),
        raw: Some(raw(&envelope.ml)?),
    });

    // llm: scan's interpreter. Its grade is its own opinion; how far that
    // moved the verdict is scan's business and shows in the verdict, not here.
    let llm = match &envelope.llm {
        Some(interpretation) => match interpretation.grade {
            Some(grade) => Judge::Ok(Assessment {
                severity: grade.into(),
                fires_at: None,
                confidence: Some(interpretation.blended),
                duration_ms: Some(interpret_ms),
                version: interpretation.model.clone(),
                raw: Some(raw(interpretation)?),
            }),
            // An interpretation with no grade is a call that failed; scan
            // records why and keeps the ML verdict.
            None => Judge::Error {
                reason: interpretation
                    .error
                    .clone()
                    .unwrap_or_else(|| "interpreter returned no grade".to_owned()),
                duration_ms: interpret_ms,
            },
        },
        None => Judge::Skipped {
            reason: llm_skipped,
        },
    };

    // The interpreter's phrase is read out before the judgement is consumed;
    // everything else about it moves into the report.
    let diff_nature = isomer
        .as_ref()
        .and_then(|j| j.interpretation.as_ref())
        .map(|i| i.nature.trim().to_owned())
        .filter(|s| !s.is_empty());

    let (diff, diff_llm, isomer_outcome) = match isomer {
        Some(judgement) => {
            let deterministic = isomer_severity(judgement.deterministic);
            let final_severity = isomer_severity(judgement.severity);
            // A differential verdict carries no measured level — nothing about
            // a change was calibrated against a benign corpus. It is placed
            // inside its band the way scan places its own interpreted
            // verdicts, counting scan's own alarm as corroboration.
            let corroborated = scan_outcome.severity > Severity::Benign;
            let diff = Judge::Ok(Assessment {
                severity: deterministic,
                fires_at: band_level(model, deterministic, corroborated),
                confidence: None,
                duration_ms: None,
                version: engines.isomer.clone(),
                // Moved, not copied. Isomer hands over its envelope already
                // serialized and it is the largest thing in the report, so
                // round-tripping it through a string to re-validate bytes we
                // just produced would be the most expensive no-op here.
                raw: Some(judgement.report),
            });
            let diff_llm = match judgement.interpretation {
                Some(interpretation) => Judge::Ok(Assessment {
                    severity: isomer_severity(interpretation.severity()),
                    fires_at: None,
                    confidence: None,
                    duration_ms: None,
                    version: interpretation.model.clone(),
                    raw: Some(raw(&interpretation)?),
                }),
                None => Judge::Skipped {
                    reason: Skipped::NotConfigured,
                },
            };
            let outcome = Outcome {
                severity: final_severity,
                fires_at: band_level(model, final_severity, corroborated),
            };
            (diff, diff_llm, Some(outcome))
        }
        // No baseline is the ordinary case, not a failure: most artifacts are
        // the first release of themselves that the corpus has seen.
        None => {
            let reason = if baseline.is_some() {
                Skipped::Unavailable
            } else {
                Skipped::NoBaseline
            };
            (Judge::Skipped { reason }, Judge::Skipped { reason }, None)
        }
    };

    let folded = combine::verdict(scan_outcome, isomer_outcome);
    let bands = [
        (JudgeId::Cleave, cleave.severity()),
        (JudgeId::Ml, ml.severity()),
        (JudgeId::Llm, llm.severity()),
        (JudgeId::Diff, diff.severity()),
        (JudgeId::DiffLlm, diff_llm.severity()),
    ];

    Ok(Report {
        version: SCHEMA_VERSION,
        sha256,
        purl,
        baseline,
        verdict: Verdict {
            severity: folded.severity,
            fires_at: folded.fires_at,
            reason: reason(&bands, folded.severity, envelope.llm.as_ref(), diff_nature),
            findings,
            decided_by: combine::decided_by(folded.severity, bands),
            engines,
            analyzed_at,
            duration_ms,
        },
        cleave,
        ml,
        llm,
        diff,
        diff_llm,
    })
}

/// The one sentence a reader gets, from whichever interpreter earned it.
///
/// An interpreter that reached the verdict is quoted ahead of one that did
/// not: when the differential is what convicted a release, "what changed" is
/// the useful sentence, and when the file itself convicted it, "what it does"
/// is. Neither is invented — postdoc only chooses which to show.
fn reason(
    bands: &[(JudgeId, Option<Severity>)],
    verdict: Severity,
    llm: Option<&scan::interpret::Interpretation>,
    diff_nature: Option<String>,
) -> Option<String> {
    let credited = |id: JudgeId| {
        bands
            .iter()
            .any(|&(candidate, band)| candidate == id && band == Some(verdict))
    };
    let from_scan = || {
        llm.map(|i| i.interpretation.trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    let from_isomer = || diff_nature;

    if credited(JudgeId::DiffLlm) && !credited(JudgeId::Llm) {
        return from_isomer().or_else(from_scan);
    }
    from_scan().or_else(from_isomer)
}

/// Where a verdict with no measured level sits inside its band.
fn band_level(
    model: &Model,
    severity: Severity,
    corroborated: bool,
) -> Option<crate::report::Level> {
    level(synthesized_level(
        model,
        classification(severity),
        corroborated,
    ))
}

/// One of scan's stored findings, as postdoc reports it.
///
/// `artifact` is the coordinate the whole report is about. A finding names a
/// package only when it is a *different* one — a fetched dependency — because
/// repeating the artifact's own coordinate on each of its findings says
/// nothing the report has not already said at the top.
fn finding(hit: scan::lookup::Hit, artifact: Option<&str>) -> Finding {
    let some = |s: String| if s.is_empty() { None } else { Some(s) };
    Finding {
        id: hit.id,
        crit: hit.crit,
        file: member_path(&hit.file),
        pkg: some(hit.pkg).filter(|pkg| Some(pkg.as_str()) != artifact),
        desc: some(hit.desc),
        off: hit.off,
        line: hit.line,
    }
}

/// Where inside the artifact a finding sits, with the analyzing host's own
/// filesystem stripped off.
///
/// Cleave names a member by the path it analyzed, which begins with wherever
/// the artifact happened to be on disk — a worker's spool directory, a
/// temporary file, a corpus mount. Three reasons that must not reach a
/// report: it discloses the layout of the machine that did the work, it says
/// nothing a consumer can use, and it makes the same artifact produce
/// different findings on different workers, so verdicts stop comparing and
/// deduplicating. Everything after the first `!!` is the part that belongs to
/// the artifact and is identical wherever it was analyzed.
///
/// A finding on the artifact itself has no member path, and `None` is the
/// honest answer: the report already names the artifact by digest.
fn member_path(path: &str) -> Option<String> {
    let (_host, member) = path.split_once("!!")?;
    let member = member.trim();
    (!member.is_empty()).then(|| member.to_owned())
}

/// An engine's output, pre-serialized for embedding.
fn raw<T: serde::Serialize>(
    value: &T,
) -> Result<Box<serde_json::value::RawValue>, serde_json::Error> {
    serde_json::value::to_raw_value(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bands(
        cleave: Option<Severity>,
        ml: Option<Severity>,
        llm: Option<Severity>,
        diff: Option<Severity>,
        diff_llm: Option<Severity>,
    ) -> [(JudgeId, Option<Severity>); 5] {
        [
            (JudgeId::Cleave, cleave),
            (JudgeId::Ml, ml),
            (JudgeId::Llm, llm),
            (JudgeId::Diff, diff),
            (JudgeId::DiffLlm, diff_llm),
        ]
    }

    #[test]
    fn a_file_verdict_quotes_the_file_interpreter() {
        // Both spoke and both reached the verdict: the sentence about what the
        // artifact does is the one a reader can act on first.
        let bands = bands(
            None,
            Some(Severity::Hostile),
            Some(Severity::Hostile),
            Some(Severity::Hostile),
            Some(Severity::Hostile),
        );
        assert_eq!(
            reason(&bands, Severity::Hostile, None, None),
            None,
            "no interpreter ran, so there is nothing to quote"
        );
    }

    #[test]
    fn nothing_is_quoted_when_no_interpreter_spoke() {
        let bands = bands(None, Some(Severity::Benign), None, None, None);
        assert_eq!(reason(&bands, Severity::Benign, None, None), None);
    }

    #[test]
    fn an_empty_field_is_absent_rather_than_blank() {
        let hit = scan::lookup::Hit {
            id: "objectives/execution/shell/bash".to_owned(),
            crit: 5,
            file: String::new(),
            pkg: String::new(),
            desc: "runs a shell".to_owned(),
            off: None,
            line: Some(2),
        };
        let reported = finding(hit, None);
        assert!(reported.file.is_none(), "an empty path is not a path");
        assert!(reported.pkg.is_none());
        assert_eq!(reported.desc.as_deref(), Some("runs a shell"));
        assert_eq!(reported.line, Some(2));
    }

    #[test]
    fn a_finding_names_a_package_only_when_it_is_a_different_one() {
        let hit = |pkg: &str| scan::lookup::Hit {
            id: "x".to_owned(),
            crit: 5,
            file: String::new(),
            pkg: pkg.to_owned(),
            desc: String::new(),
            off: None,
            line: None,
        };
        let artifact = "pkg:npm/left-pad@1.3.1";

        assert_eq!(
            finding(hit(artifact), Some(artifact)).pkg,
            None,
            "the report already names the artifact; every finding repeating it is noise"
        );
        assert_eq!(
            finding(hit("pkg:npm/evil@9.9.9"), Some(artifact))
                .pkg
                .as_deref(),
            Some("pkg:npm/evil@9.9.9"),
            "a finding inside a fetched dependency must name it"
        );
    }

    #[test]
    fn a_finding_never_carries_the_analyzing_hosts_filesystem() {
        // The leading path is wherever the worker happened to put the bytes.
        // It discloses that machine's layout, and it makes one artifact
        // produce different findings on different workers.
        assert_eq!(
            member_path("/var/spool/hopper/data/ab/cd/pkg.tgz!!package/lib/.cache.js"),
            Some("package/lib/.cache.js".to_owned())
        );
        assert_eq!(
            member_path("/tmp/scan-7f3a/x.tgz!!package/a.js!!package/a.js##base64@96"),
            Some("package/a.js!!package/a.js##base64@96".to_owned()),
            "cleave's decoded-payload notation is the artifact's, and is kept"
        );
    }

    #[test]
    fn a_finding_on_the_artifact_itself_names_no_member() {
        // The report already identifies the artifact by digest; repeating a
        // local path here would say nothing and disclose something.
        assert_eq!(member_path("/var/spool/hopper/data/ab/cd/pkg.tgz"), None);
        assert_eq!(member_path(""), None);
        assert_eq!(member_path("/tmp/x.tgz!!"), None);
    }

    #[test]
    fn the_same_artifact_reports_the_same_findings_anywhere() {
        // Two workers, two spool directories, one artifact. The reported
        // location has to match, or the corpus cannot deduplicate a verdict.
        let one = "/var/spool/a/1234/pkg.tgz!!package/lib/.cache.js";
        let other = "/srv/hopper/data/zz/pkg.tgz!!package/lib/.cache.js";
        assert_eq!(member_path(one), member_path(other));
    }

    #[test]
    fn engine_output_is_embedded_without_re_encoding() {
        // Key order and spacing belong to the engine, not to serde. An
        // engine's output reaches the report byte for byte, which is what
        // lets the diff judge move isomer's envelope in rather than copy it.
        let exact = r#"{"z":1,"a":[2,3]}"#;
        let output: &serde_json::value::RawValue = serde_json::from_str(exact).unwrap();
        assert_eq!(raw(&output).unwrap().get(), exact);
    }
}
