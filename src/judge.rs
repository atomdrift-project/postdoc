//! Folding isomer's opinion of a release into scan's upload.
//!
//! Scan writes the upload (version 8, scan's `JSON_PLAN.md`): its verdict on
//! the artifact and everything inside it, and its own three jurors. When
//! hopper named an earlier release, isomer judges the difference, and its two
//! opinions join scan's as the artifact's `drift_heuristic` and `drift_llm` jurors.
//! The verdict is the worse of scan's and isomer's ([`combine::verdict`]).
//!
//! Nothing here grades: every band is one an engine settled on, and the only
//! choices postdoc makes are which outcome is worse and which juror decided
//! it.

use scan::engine::{Juror, OpinionError, ScanResult, synthesized_level};
use scan::model::Model;
use serde_json::json;

use crate::bounded;
use crate::combine::{self, Outcome};
use crate::engine::{classification, isomer_severity, level, scan_level};
use crate::report::Severity;
use crate::result::Baseline;

/// The most bytes of isomer's interpreter's phrase an upload carries: what
/// scan allows any juror's free text, so the object stays inside hopper's
/// limit.
const MAX_REASON: usize = 400;

/// Everything postdoc knows about one artifact.
#[derive(Debug)]
pub struct Analysis<'a> {
    /// What scan concluded, classified for upload. Taken by value because it
    /// is what is returned, with isomer's opinions added.
    pub scan: ScanResult,
    /// What isomer concluded about the change from [`Self::baseline`], when
    /// there was a baseline to compare against.
    pub isomer: Option<isomer::judgement::Judgement>,
    /// The release isomer compared against.
    pub baseline: Option<Baseline>,
    /// The loaded model, for placing a differential verdict inside its band.
    pub model: &'a Model,
}

/// Scan's result, with isomer's opinions added to its upload as the drift
/// jurors and the verdict folded. Unchanged when isomer did not run or the
/// result was not classified for upload.
///
/// # Errors
///
/// One of isomer's opinions does not fit on the wire.
pub fn judge(analysis: Analysis<'_>) -> Result<ScanResult, OpinionError> {
    let Analysis {
        mut scan,
        isomer,
        baseline,
        model,
    } = analysis;
    let (Some(judgement), Some(doc)) = (isomer, scan.upload.as_mut()) else {
        return Ok(scan);
    };
    let Some(verdict) = doc.verdict() else {
        return Ok(scan);
    };
    let scan_outcome = Outcome {
        severity: verdict.class.into(),
        fires_at: level(verdict.level),
    };
    // A differential verdict carries no measured level — nothing about a
    // change was calibrated against a benign corpus. It is placed inside its
    // band the way scan places its own interpreted verdicts, counting scan's
    // own alarm as corroboration.
    let corroborated = scan_outcome.severity > Severity::Benign;
    let placed =
        |severity: Severity| synthesized_level(model, classification(severity), corroborated);
    let base = baseline.as_ref().map(|b| b.sha256.as_str());

    let deterministic = isomer_severity(judgement.deterministic);
    doc.set_juror(
        Juror::DriftHeuristic,
        json!({
            "class": deterministic.as_str(),
            "fires_at": placed(deterministic),
            "base": base,
            "engine": isomer::VERSION,
        }),
    )?;
    let interpreted = judgement
        .interpretation
        .as_ref()
        .map(|i| isomer_severity(i.severity()));
    if let (Some(interpretation), Some(band)) = (&judgement.interpretation, interpreted) {
        doc.set_juror(
            Juror::DriftLlm,
            json!({
                "class": band.as_str(),
                "model": interpretation.model,
                "reason": bounded(interpretation.nature.trim(), MAX_REASON),
                "base": base,
            }),
        )?;
    }

    let finished = isomer_severity(judgement.severity);
    let isomer_outcome = Outcome {
        severity: finished,
        fires_at: level(placed(finished)),
    };
    if let Some((folded, by)) = decision(scan_outcome, isomer_outcome, deterministic, interpreted) {
        doc.decide(
            classification(folded.severity),
            scan_level(folded.fires_at),
            by,
        );
    }
    Ok(scan)
}

/// The verdict to record and the juror that decided it, when isomer's
/// outcome is worse than scan's; `None` when scan's stands.
///
/// isomer's outcome is its differential reading with its own interpreter
/// already applied. The interpreter is credited when it is what moved isomer
/// to that band, and the differential otherwise.
fn decision(
    scan: Outcome,
    isomer: Outcome,
    deterministic: Severity,
    interpreted: Option<Severity>,
) -> Option<(Outcome, Juror)> {
    let folded = combine::verdict(scan, Some(isomer));
    if folded == scan {
        return None;
    }
    let by = if interpreted == Some(isomer.severity) && deterministic != isomer.severity {
        Juror::DriftLlm
    } else {
        Juror::DriftHeuristic
    };
    Some((folded, by))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::Level;

    fn at(severity: Severity, budget: u16) -> Outcome {
        Outcome {
            severity,
            fires_at: Some(Level::At(budget)),
        }
    }

    fn clean() -> Outcome {
        Outcome {
            severity: Severity::Benign,
            fires_at: Some(Level::Never),
        }
    }

    #[test]
    fn scans_verdict_stands_unless_isomers_is_worse() {
        let scan = at(Severity::Hostile, 12);
        assert_eq!(
            decision(
                scan,
                at(Severity::Suspicious, 500),
                Severity::Suspicious,
                None
            ),
            None
        );
        assert_eq!(
            decision(scan, at(Severity::Hostile, 25), Severity::Hostile, None),
            None,
            "a tie keeps scan's tighter budget"
        );
    }

    #[test]
    fn the_differential_decides_when_it_reached_the_band_itself() {
        let isomer = at(Severity::Hostile, 25);
        assert_eq!(
            decision(clean(), isomer, Severity::Hostile, Some(Severity::Hostile)),
            Some((isomer, Juror::DriftHeuristic))
        );
        assert_eq!(
            decision(clean(), isomer, Severity::Hostile, None),
            Some((isomer, Juror::DriftHeuristic))
        );
    }

    #[test]
    fn the_interpreter_decides_when_it_moved_isomer_to_the_band() {
        let isomer = at(Severity::Suspicious, 2500);
        assert_eq!(
            decision(
                clean(),
                isomer,
                Severity::Benign,
                Some(Severity::Suspicious)
            ),
            Some((isomer, Juror::DriftLlm))
        );
    }
}
