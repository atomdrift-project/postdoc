//! The wire shape of a postdoc result.
//!
//! One analyzed artifact produces one result: a [`Verdict`] and, beside it,
//! one [`Judge`] per engine that was asked. These types are the contract
//! hopper stores and beamline reads, so they are deliberately plain — every
//! field is data an engine reported, and nothing here computes a severity.
//! Folding the judges into the verdict is [`crate::combine`]'s job, and even
//! that only picks between outcomes the engines already decided.
//!
//! The concrete engine payloads that fill [`Assessment::raw`] are named where
//! postdoc links those engines; everything in this module is generic over
//! them, so the shape can be reviewed and tested without an analysis stack.

use serde::de::{Error as _, Unexpected};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// How bad an artifact is, independent of any caller's threshold.
///
/// The same three bands scan classifies into and beamline reports. Ordered
/// from least to most severe, so `max` is the worse of two.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Nothing worth acting on.
    #[default]
    Benign,
    /// Worth a human's attention, not a block on its own.
    Suspicious,
    /// Almost certainly malicious.
    Hostile,
}

impl Severity {
    /// Every band, least severe first.
    pub const ALL: [Self; 3] = [Self::Benign, Self::Suspicious, Self::Hostile];

    /// The lowercase wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Benign => "benign",
            Self::Suspicious => "suspicious",
            Self::Hostile => "hostile",
        }
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The tightest false-positive budget, per 100 million benign files, at which
/// an artifact grades hostile.
///
/// Lower is worse. An artifact that fires at 1 is convicted even by a caller
/// who tolerates almost no false positives; one that fires at 3000 is
/// convicted only by a caller who tolerates many. The value is *measured* by
/// the engine that produced it and does not depend on the asking caller's
/// budget — turning it into an allow or a block is the caller's business, and
/// postdoc never does it.
///
/// Scan calls this `lvl` and hopper calls it `fires_at`; both encode "never"
/// as `-1`, and this type serializes the same way so the three agree on the
/// wire. A level is absent entirely (`None` at the field) when the engine ran
/// under manual thresholds and no calibrated budget applies.
///
/// Deliberately not `Ord`: "worse" runs opposite to numeric order, and a
/// derived comparison would quietly rank a clean artifact above a convicted
/// one. Use [`Level::worse_of`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// Fires at this budget.
    At(u16),
    /// Fires at no calibrated budget.
    Never,
}

impl Level {
    /// The worse of two levels.
    ///
    /// Firing anywhere beats never firing, and among those that fire the
    /// tighter budget wins.
    #[must_use]
    pub const fn worse_of(self, other: Self) -> Self {
        match (self, other) {
            (Self::At(a), Self::At(b)) => Self::At(if a <= b { a } else { b }),
            (Self::At(_), Self::Never) => self,
            (Self::Never, _) => other,
        }
    }
}

impl Serialize for Level {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // `-1` for `Never`, else the budget.
        serializer.serialize_i32(match *self {
            Self::At(budget) => i32::from(budget),
            Self::Never => -1,
        })
    }
}

impl<'de> Deserialize<'de> for Level {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = i32::deserialize(deserializer)?;
        if raw < 0 {
            return Ok(Self::Never);
        }
        // Every calibrated grid tops out well inside u16 (scan's is 25000), so
        // a larger number is a different scale, not a looser budget. Refusing
        // it beats saturating it into a verdict nobody measured.
        u16::try_from(raw).map(Self::At).map_err(|_| {
            D::Error::invalid_value(
                Unexpected::Signed(i64::from(raw)),
                &"a false-positive budget of -1 or 0..=65535",
            )
        })
    }
}

/// Which engine an opinion came from.
///
/// The key a judge appears under in the result object, and the vocabulary of
/// [`Verdict::decided_by`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JudgeId {
    /// Cleave's traits, graded by scan's trait floor.
    Cleave,
    /// The azoth model, read through scan.
    Ml,
    /// Scan's per-file interpreter.
    Llm,
    /// Isomer, over this artifact and an earlier release of the same package.
    Diff,
    /// Isomer's per-diff interpreter.
    DiffLlm,
}

impl JudgeId {
    /// Every judge, in the order they appear in a result.
    pub const ALL: [Self; 5] = [Self::Cleave, Self::Ml, Self::Llm, Self::Diff, Self::DiffLlm];

    /// The snake_case wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cleave => "cleave",
            Self::Ml => "ml",
            Self::Llm => "llm",
            Self::Diff => "diff",
            Self::DiffLlm => "diff_llm",
        }
    }
}

impl std::fmt::Display for JudgeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why an engine was not asked.
///
/// A closed set, because consumers branch on it. An engine that *was* asked
/// and failed carries a free-text reason instead, under [`Judge::Error`] —
/// failures are open-ended in a way that "we did not ask" is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Skipped {
    /// No earlier release of this package to compare against.
    NoBaseline,
    /// The engine is not configured in this deployment.
    NotConfigured,
    /// The engine's own admission gate declined this artifact.
    NotAdmitted,
    /// The engine is configured but was not reachable or had no capacity.
    Unavailable,
}

impl Skipped {
    /// Every reason, in the order they are declared.
    pub const ALL: [Self; 4] = [
        Self::NoBaseline,
        Self::NotConfigured,
        Self::NotAdmitted,
        Self::Unavailable,
    ];

    /// The wire spellings, for an error message naming what was expected.
    pub const NAMES: &'static [&'static str] = &[
        "no_baseline",
        "not_configured",
        "not_admitted",
        "unavailable",
    ];

    /// The reason a wire spelling names, or `None` if it names none of them.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == s)
    }

    /// The snake_case wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoBaseline => "no_baseline",
            Self::NotConfigured => "not_configured",
            Self::NotAdmitted => "not_admitted",
            Self::Unavailable => "unavailable",
        }
    }
}

impl std::fmt::Display for Skipped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What one engine reported.
///
/// `raw` is that engine's own output, verbatim. postdoc never reads inside
/// it; it is carried so a consumer can see the evidence behind the summary
/// above it, and it is dropped on responses that did not ask for it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Assessment<R> {
    /// The band this engine put the artifact in.
    pub severity: Severity,
    /// The budget at which it grades hostile, when the engine measures one.
    pub fires_at: Option<Level>,
    /// The engine's own confidence, where it reports one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    /// Wall-clock time this engine took, where it is separately measured.
    ///
    /// Omitted rather than guessed. Some engines share a call: cleave's
    /// analysis and the model's inference happen inside one pass through
    /// scan and are not split, and isomer's interpreter runs inside its
    /// judgement. Reporting an invented share of a combined measurement
    /// would read like a real one to whoever is chasing a slow analysis.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// The engine build, model, or ruleset that produced this.
    pub version: String,
    /// The engine's native output. Absent when the caller did not ask for it.
    ///
    /// No `#[serde(default)]`: on a generic field that would demand
    /// `R: Default` from every engine payload, and serde already reads a
    /// missing `Option` field as `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<R>,
}

/// One engine's slot in a result.
///
/// Every judge key is present in every result, so a consumer writes one code
/// path. The `status` field discriminates: an engine either reported, was not
/// asked, or failed.
///
/// Serialization is derived; deserialization is hand-written just below.
/// Serde's internally-tagged representation buffers a value before dispatching
/// on its tag, and a buffered value is no longer the caller's original input —
/// which is exactly what a pre-serialized payload needs to borrow. Parsing
/// through a flat intermediate keeps `raw` readable and moves the invariants
/// ("reported" implies a severity, and so on) to the boundary, where a
/// malformed document is rejected rather than half-accepted.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Judge<R> {
    /// The engine ran and reported.
    Ok(Assessment<R>),
    /// The engine was not asked.
    Skipped {
        /// Why it was not asked.
        reason: Skipped,
    },
    /// The engine was asked and failed.
    Error {
        /// What went wrong.
        reason: String,
        /// How long it ran before failing. An engine that fails slowly and one
        /// that fails immediately are different operational problems.
        duration_ms: u64,
    },
}

impl<R> Judge<R> {
    /// The engine's assessment, if it reported one.
    #[must_use]
    pub const fn assessment(&self) -> Option<&Assessment<R>> {
        match self {
            Self::Ok(assessment) => Some(assessment),
            Self::Skipped { .. } | Self::Error { .. } => None,
        }
    }

    /// The band this engine reported, if it reported.
    #[must_use]
    pub const fn severity(&self) -> Option<Severity> {
        match self {
            Self::Ok(assessment) => Some(assessment.severity),
            Self::Skipped { .. } | Self::Error { .. } => None,
        }
    }

    /// Drop the engine's native output, keeping the summary.
    ///
    /// What a response without `full=1` returns.
    pub fn strip_raw(&mut self) {
        if let Self::Ok(assessment) = self {
            assessment.raw = None;
        }
    }
}

/// What a judge's `status` said, before the rest of it is checked.
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    Ok,
    Skipped,
    Error,
}

/// A judge as it lies on the wire, before its invariants are checked.
///
/// Flat and permissive on purpose: this shape exists only so serde hands the
/// real deserializer to the `raw` field. Every field is `Option`, which serde
/// already reads as absent-means-`None`, so none of them needs a `default`. [`Judge`]'s own `Deserialize` turns
/// one of these into a value whose status and contents cannot disagree.
#[derive(Deserialize)]
struct JudgeWire<R> {
    status: Status,
    reason: Option<String>,
    severity: Option<Severity>,
    fires_at: Option<Level>,
    confidence: Option<f32>,
    duration_ms: Option<u64>,
    version: Option<String>,
    raw: Option<R>,
}

impl<'de, R: Deserialize<'de>> Deserialize<'de> for Judge<R> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = JudgeWire::<R>::deserialize(deserializer)?;
        match wire.status {
            Status::Ok => Ok(Self::Ok(Assessment {
                severity: wire
                    .severity
                    .ok_or_else(|| D::Error::missing_field("severity"))?,
                fires_at: wire.fires_at,
                confidence: wire.confidence,
                duration_ms: wire.duration_ms,
                version: wire
                    .version
                    .ok_or_else(|| D::Error::missing_field("version"))?,
                raw: wire.raw,
            })),
            Status::Skipped => {
                let reason = wire
                    .reason
                    .ok_or_else(|| D::Error::missing_field("reason"))?;
                Ok(Self::Skipped {
                    reason: Skipped::parse(&reason)
                        .ok_or_else(|| D::Error::unknown_variant(&reason, Skipped::NAMES))?,
                })
            }
            Status::Error => Ok(Self::Error {
                reason: wire
                    .reason
                    .ok_or_else(|| D::Error::missing_field("reason"))?,
                duration_ms: wire
                    .duration_ms
                    .ok_or_else(|| D::Error::missing_field("duration_ms"))?,
            }),
        }
    }
}

/// One finding, flattened to what a consumer gates on.
///
/// The same shape scan's `/v1/lookup` answers with, so a reader that already
/// parses that parses this.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// Stable trait identifier, such as `objectives/execution/shell/bash`.
    pub id: String,
    /// Criticality ordinal: 3 notable, 4 suspicious, 5 hostile.
    pub crit: u8,
    /// The member file it fired on, within an archive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// The package the member belongs to, when it is a dependency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pkg: Option<String>,
    /// One-line description of the trait.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desc: Option<String>,
    /// Byte offset of the match.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub off: Option<u64>,
    /// Line number of the match, in text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
}

/// The builds that produced a result.
///
/// Recorded per result rather than per deployment: a corpus holds verdicts
/// from many builds at once, and re-analysis decisions turn on which one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Engines {
    /// This crate's version.
    pub postdoc: String,
    /// The scan build.
    pub scan: String,
    /// The cleave build.
    pub cleave: String,
    /// The isomer build.
    pub isomer: String,
    /// The traits bundle revision.
    pub traits: String,
    /// The model bundle identifier.
    pub model: String,
}

/// What postdoc concluded, and from whom.
///
/// Scan's stored verdict shape without `decision`: whether to allow or block
/// depends on the asking caller's false-positive budget, so it is computed
/// when a lookup is answered and never stored.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    /// The band, folded from the engines' own outcomes.
    pub severity: Severity,
    /// The budget at which this artifact grades hostile.
    pub fires_at: Option<Level>,
    /// One sentence from whichever interpreter spoke, when one did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The worst findings, worst first.
    pub findings: Vec<Finding>,
    /// The judges whose own band equals [`Verdict::severity`].
    pub decided_by: Vec<JudgeId>,
    /// The builds behind this result.
    pub engines: Engines,
    /// RFC 3339 timestamp, UTC, of when the analysis ran.
    pub analyzed_at: String,
    /// Wall-clock time for the whole analysis, judges included.
    pub duration_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The raw payloads are engine types postdoc only forwards; a map stands
    /// in for one here so the envelope can be exercised on its own.
    type Raw = std::collections::BTreeMap<String, u32>;

    fn raw() -> Raw {
        Raw::from([("files".to_owned(), 2)])
    }

    #[test]
    fn severity_orders_least_to_most_severe() {
        assert!(Severity::Benign < Severity::Suspicious);
        assert!(Severity::Suspicious < Severity::Hostile);
        assert_eq!(
            Severity::ALL.iter().copied().max(),
            Some(Severity::Hostile),
            "max over every band is the worst one"
        );
    }

    #[test]
    fn severity_wire_spelling_is_lowercase() {
        for band in Severity::ALL {
            let json = serde_json::to_string(&band).unwrap();
            assert_eq!(json, format!("\"{}\"", band.as_str()));
            assert_eq!(serde_json::from_str::<Severity>(&json).unwrap(), band);
        }
    }

    #[test]
    fn level_encodes_never_as_minus_one() {
        assert_eq!(serde_json::to_string(&Level::Never).unwrap(), "-1");
        assert_eq!(serde_json::to_string(&Level::At(25)).unwrap(), "25");
        assert_eq!(serde_json::to_string(&Level::At(0)).unwrap(), "0");
    }

    #[test]
    fn level_round_trips_through_the_wire() {
        for level in [Level::Never, Level::At(0), Level::At(25), Level::At(25_000)] {
            let json = serde_json::to_string(&level).unwrap();
            assert_eq!(serde_json::from_str::<Level>(&json).unwrap(), level);
        }
    }

    #[test]
    fn level_reads_any_negative_as_never() {
        // Scan writes -1, but nothing downstream should depend on which
        // negative number an older build chose.
        assert_eq!(serde_json::from_str::<Level>("-1").unwrap(), Level::Never);
        assert_eq!(serde_json::from_str::<Level>("-99").unwrap(), Level::Never);
    }

    #[test]
    fn level_refuses_a_budget_off_the_calibrated_scale() {
        let err = serde_json::from_str::<Level>("70000").unwrap_err();
        assert!(
            err.to_string().contains("false-positive budget"),
            "error should name the scale, got: {err}"
        );
    }

    #[test]
    fn worse_of_prefers_the_tighter_budget() {
        assert_eq!(Level::At(25).worse_of(Level::At(3000)), Level::At(25));
        assert_eq!(Level::At(3000).worse_of(Level::At(25)), Level::At(25));
        assert_eq!(Level::At(0).worse_of(Level::At(25)), Level::At(0));
    }

    #[test]
    fn worse_of_prefers_firing_over_never_firing() {
        assert_eq!(Level::Never.worse_of(Level::At(3000)), Level::At(3000));
        assert_eq!(Level::At(3000).worse_of(Level::Never), Level::At(3000));
        assert_eq!(Level::Never.worse_of(Level::Never), Level::Never);
    }

    #[test]
    fn worse_of_is_commutative_and_idempotent() {
        let levels = [Level::Never, Level::At(0), Level::At(25), Level::At(3000)];
        for a in levels {
            assert_eq!(a.worse_of(a), a, "idempotent on {a:?}");
            for b in levels {
                assert_eq!(
                    a.worse_of(b),
                    b.worse_of(a),
                    "worse_of({a:?}, {b:?}) must not depend on argument order"
                );
            }
        }
    }

    #[test]
    fn an_ok_judge_carries_its_evidence() {
        let judge = Judge::Ok(Assessment {
            severity: Severity::Hostile,
            fires_at: Some(Level::At(25)),
            confidence: Some(0.93),
            duration_ms: Some(80),
            version: "2.12.0".to_owned(),
            raw: Some(raw()),
        });

        let json = serde_json::to_value(&judge).unwrap();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["severity"], "hostile");
        assert_eq!(json["fires_at"], 25);
        assert_eq!(json["raw"]["files"], 2);
        assert_eq!(serde_json::from_value::<Judge<Raw>>(json).unwrap(), judge);
    }

    #[test]
    fn fires_at_is_present_as_null_when_no_level_applies() {
        // A judge that grades without a calibrated budget — the trait floor on
        // a manual-threshold deployment — still has the key, so a consumer
        // never has to tell "absent" from "no level".
        let judge: Judge<Raw> = Judge::Ok(Assessment {
            severity: Severity::Benign,
            fires_at: None,
            confidence: None,
            duration_ms: Some(900),
            version: "abc12345".to_owned(),
            raw: None,
        });

        let json = serde_json::to_value(&judge).unwrap();
        assert!(json.get("fires_at").is_some(), "the key must be present");
        assert!(json["fires_at"].is_null());
        assert!(json.get("confidence").is_none(), "absent, not null");
        assert!(json.get("raw").is_none(), "absent, not null");
        assert_eq!(
            serde_json::from_value::<Judge<Raw>>(json).unwrap(),
            judge,
            "a judge whose evidence was stripped must still parse"
        );
    }

    #[test]
    fn a_skipped_judge_says_only_why() {
        let judge: Judge<Raw> = Judge::Skipped {
            reason: Skipped::NoBaseline,
        };

        let json = serde_json::to_value(&judge).unwrap();
        assert_eq!(json["status"], "skipped");
        assert_eq!(json["reason"], "no_baseline");
        assert_eq!(
            json.as_object().map(serde_json::Map::len),
            Some(2),
            "nothing ran, so there is nothing else to report"
        );
        assert_eq!(serde_json::from_value::<Judge<Raw>>(json).unwrap(), judge);
    }

    #[test]
    fn an_errored_judge_reports_how_long_it_ran() {
        let judge: Judge<Raw> = Judge::Error {
            reason: "llm endpoint timed out".to_owned(),
            duration_ms: 60_000,
        };

        let json = serde_json::to_value(&judge).unwrap();
        assert_eq!(json["status"], "error");
        assert_eq!(json["duration_ms"], 60_000);
        assert_eq!(serde_json::from_value::<Judge<Raw>>(json).unwrap(), judge);
    }

    #[test]
    fn severity_is_absent_unless_an_engine_reported_one() {
        let ok = Judge::Ok(Assessment {
            severity: Severity::Suspicious,
            fires_at: Some(Level::At(3000)),
            confidence: None,
            duration_ms: Some(1),
            version: "v".to_owned(),
            raw: None::<Raw>,
        });
        assert_eq!(ok.severity(), Some(Severity::Suspicious));
        assert!(ok.assessment().is_some());

        let skipped = Judge::Skipped::<Raw> {
            reason: Skipped::NotConfigured,
        };
        assert_eq!(skipped.severity(), None);
        assert!(skipped.assessment().is_none());

        let errored = Judge::Error::<Raw> {
            reason: "boom".to_owned(),
            duration_ms: 3,
        };
        assert_eq!(errored.severity(), None);
        assert!(errored.assessment().is_none());
    }

    #[test]
    fn strip_raw_keeps_the_summary() {
        let mut judge = Judge::Ok(Assessment {
            severity: Severity::Hostile,
            fires_at: Some(Level::At(25)),
            confidence: Some(0.9),
            duration_ms: Some(80),
            version: "2.12.0".to_owned(),
            raw: Some(raw()),
        });

        judge.strip_raw();

        let Judge::Ok(assessment) = &judge else {
            panic!("stripping evidence must not change the status");
        };
        assert!(assessment.raw.is_none());
        assert_eq!(assessment.severity, Severity::Hostile);
        assert_eq!(assessment.fires_at, Some(Level::At(25)));
    }

    #[test]
    fn strip_raw_leaves_a_judge_that_never_ran_alone() {
        let mut judge = Judge::Skipped::<Raw> {
            reason: Skipped::NoBaseline,
        };
        judge.strip_raw();
        assert_eq!(
            judge,
            Judge::Skipped {
                reason: Skipped::NoBaseline
            }
        );
    }

    #[test]
    fn a_reported_judge_missing_its_verdict_is_refused() {
        // The invariants the enum encodes are checked where a document enters,
        // so no half-built judge exists further in. A consumer that reads
        // `status: "ok"` is entitled to a severity.
        for (missing, doc) in [
            (
                "severity",
                r#"{"status":"ok","duration_ms":1,"version":"v"}"#,
            ),
            (
                "version",
                r#"{"status":"ok","severity":"benign","duration_ms":1}"#,
            ),
        ] {
            let err = serde_json::from_str::<Judge<Raw>>(doc).unwrap_err();
            assert!(
                err.to_string().contains(missing),
                "expected the error to name `{missing}`, got: {err}"
            );
        }
    }

    #[test]
    fn a_timing_nobody_measured_is_absent_rather_than_zero() {
        // Zero would read as "instant" to whoever is chasing a slow analysis.
        let judge: Judge<Raw> = Judge::Ok(Assessment {
            severity: Severity::Benign,
            fires_at: Some(Level::Never),
            confidence: None,
            duration_ms: None,
            version: "v".to_owned(),
            raw: None,
        });
        let json = serde_json::to_value(&judge).unwrap();
        assert!(json.get("duration_ms").is_none(), "absent, not zero");
        assert_eq!(serde_json::from_value::<Judge<Raw>>(json).unwrap(), judge);
    }

    #[test]
    fn a_skip_reason_outside_the_closed_set_is_refused() {
        // Consumers branch on this value. Silently admitting one they have
        // never heard of would send them down a default path for a reason
        // that might matter.
        let err = serde_json::from_str::<Judge<Raw>>(r#"{"status":"skipped","reason":"bored"}"#)
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("bored"), "got: {message}");
        assert!(message.contains("no_baseline"), "got: {message}");
    }

    #[test]
    fn an_unknown_status_is_refused() {
        let err = serde_json::from_str::<Judge<Raw>>(r#"{"status":"maybe","severity":"benign"}"#)
            .unwrap_err();
        assert!(err.to_string().contains("maybe"), "got: {err}");
    }

    #[test]
    fn a_failed_judge_must_say_how_long_it_ran() {
        let err = serde_json::from_str::<Judge<Raw>>(r#"{"status":"error","reason":"boom"}"#)
            .unwrap_err();
        assert!(err.to_string().contains("duration_ms"), "got: {err}");
    }

    #[test]
    fn every_skip_reason_parses_back_from_its_own_spelling() {
        for reason in Skipped::ALL {
            assert_eq!(Skipped::parse(reason.as_str()), Some(reason));
        }
        assert_eq!(Skipped::parse("no such reason"), None);
        assert_eq!(
            Skipped::NAMES.len(),
            Skipped::ALL.len(),
            "the error message must name every reason that exists"
        );
        for (reason, name) in Skipped::ALL.iter().zip(Skipped::NAMES) {
            assert_eq!(&reason.as_str(), name);
        }
    }

    #[test]
    fn judge_ids_and_skip_reasons_spell_themselves_the_same_way_twice() {
        // `as_str` is what logs and metrics use; serde is what the wire uses.
        // They must not drift.
        for id in JudgeId::ALL {
            let json = serde_json::to_string(&id).unwrap();
            assert_eq!(json, format!("\"{}\"", id.as_str()));
        }
        for reason in Skipped::ALL {
            let json = serde_json::to_string(&reason).unwrap();
            assert_eq!(json, format!("\"{}\"", reason.as_str()));
        }
    }

    #[test]
    fn a_verdict_omits_only_what_it_genuinely_lacks() {
        let verdict = Verdict {
            severity: Severity::Hostile,
            fires_at: Some(Level::At(25)),
            reason: None,
            findings: vec![Finding {
                id: "objectives/execution/shell/bash".to_owned(),
                crit: 5,
                file: Some("package/postinstall.js".to_owned()),
                pkg: None,
                desc: Some("runs a shell".to_owned()),
                off: None,
                line: Some(12),
            }],
            decided_by: vec![JudgeId::Ml, JudgeId::Diff],
            engines: Engines {
                postdoc: "0.1.0".to_owned(),
                scan: "2.12.0".to_owned(),
                cleave: "2.12.0".to_owned(),
                isomer: "0.5.0".to_owned(),
                traits: "abc12345".to_owned(),
                model: "8c070d64ee9b".to_owned(),
            },
            analyzed_at: "2026-09-15T12:00:00Z".to_owned(),
            duration_ms: 4120,
        };

        let json = serde_json::to_value(&verdict).unwrap();
        assert_eq!(json["severity"], "hostile");
        assert_eq!(json["fires_at"], 25);
        assert!(json.get("reason").is_none(), "no interpreter spoke");
        assert_eq!(json["decided_by"], serde_json::json!(["ml", "diff"]));
        assert_eq!(json["findings"][0]["crit"], 5);
        assert!(
            json["findings"][0].get("pkg").is_none(),
            "a finding omits what it has nothing to say about"
        );
        assert_eq!(serde_json::from_value::<Verdict>(json).unwrap(), verdict);
    }
}
