//! One analyzed artifact, as it goes on the wire.
//!
//! [`Report`] is what `postdoc worker` posts to hopper and what `postdoc
//! serve` answers with. It names the artifact, says what was concluded, and
//! carries one [`Judge`] per engine that was asked.
//!
//! Every engine's output rides as pre-serialized JSON. postdoc never reads
//! inside it — it is evidence for whoever does — so naming five foreign types
//! here would buy nothing and would tie this contract to five crates'
//! internals. A [`serde_json::value::RawValue`] also embeds into the report
//! without a parse or a re-encode, which at worker scale is the difference
//! between touching a large cleave report once and touching it three times.

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::report::{Judge, Level, Verdict};

/// Schema version of [`Report`].
pub const SCHEMA_VERSION: &str = "1";

const fn schema_version() -> &'static str {
    SCHEMA_VERSION
}

/// One engine's slot in a report.
///
/// All five are the same type: they share an envelope, and what differs
/// between them is the engine behind it, not the shape.
pub type EngineJudge = Judge<Box<RawValue>>;

/// The earlier release an artifact was compared against.
///
/// An input postdoc chose, not a finding — which is why it sits beside the
/// artifact's own identity rather than inside the `diff` judge. Hopper picks
/// it, because hopper is the only party that knows what it holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Baseline {
    /// The bytes that were compared against.
    pub sha256: String,
    /// The coordinate of that release, when it is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purl: Option<String>,
    /// Its version string, when it is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The corpus label it carries — hopper's vocabulary, not postdoc's, so a
    /// value this build has not heard of is still reported rather than
    /// refused. A baseline may legitimately be one that was itself convicted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The budget at which the baseline itself grades hostile, if it has a
    /// verdict on file. Lets a reader weigh a comparison against a bad
    /// predecessor without a second lookup.
    pub fires_at: Option<Level>,
}

/// One analyzed artifact.
///
/// Deliberately not `PartialEq`: a judge's evidence is raw JSON, and two
/// encodings of the same document are equal as data but not as bytes. What
/// callers and tests actually compare is the serialized report, so that is
/// the only comparison offered.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    /// Schema version. Always [`SCHEMA_VERSION`] on the way out.
    #[serde(rename = "v", skip_deserializing, default = "schema_version")]
    pub version: &'static str,
    /// The analyzed bytes.
    pub sha256: String,
    /// The coordinate the artifact was fetched as, when one is known.
    pub purl: Option<String>,
    /// The release this one was compared against, when there was one.
    pub baseline: Option<Baseline>,
    /// What postdoc concluded, and from whom.
    pub verdict: Verdict,
    /// Cleave's traits, graded by scan's trait floor.
    pub cleave: EngineJudge,
    /// The azoth model, read through scan.
    pub ml: EngineJudge,
    /// Scan's per-file interpreter.
    pub llm: EngineJudge,
    /// Isomer, against [`Report::baseline`].
    pub diff: EngineJudge,
    /// Isomer's per-diff interpreter.
    pub diff_llm: EngineJudge,
}

impl Report {
    /// Drop every engine's native output, keeping the verdict and each
    /// judge's summary.
    ///
    /// What a response without `full=1` returns. The summaries are what a
    /// caller gates on; the evidence is what it asks for when it wants to
    /// know why.
    pub fn strip_raw(&mut self) {
        for judge in [
            &mut self.cleave,
            &mut self.ml,
            &mut self.llm,
            &mut self.diff,
            &mut self.diff_llm,
        ] {
            judge.strip_raw();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{Assessment, Engines, Finding, JudgeId, Severity, Skipped};

    fn raw(json: &str) -> Box<RawValue> {
        RawValue::from_string(json.to_owned()).unwrap()
    }

    fn reported(severity: Severity, fires_at: Option<Level>, json: &str) -> EngineJudge {
        Judge::Ok(Assessment {
            severity,
            fires_at,
            confidence: None,
            duration_ms: Some(10),
            version: "test".to_owned(),
            raw: Some(raw(json)),
        })
    }

    fn report() -> Report {
        Report {
            version: SCHEMA_VERSION,
            sha256: "a".repeat(64),
            purl: Some("pkg:npm/left-pad@1.3.1".to_owned()),
            baseline: Some(Baseline {
                sha256: "b".repeat(64),
                purl: Some("pkg:npm/left-pad@1.3.0".to_owned()),
                version: Some("1.3.0".to_owned()),
                label: Some("good".to_owned()),
                fires_at: Some(Level::Never),
            }),
            verdict: Verdict {
                severity: Severity::Hostile,
                fires_at: Some(Level::At(25)),
                reason: Some("postinstall fetches and executes a remote script".to_owned()),
                findings: vec![Finding {
                    id: "objectives/execution/shell/bash".to_owned(),
                    crit: 5,
                    file: Some("package/setup.js".to_owned()),
                    pkg: None,
                    desc: Some("runs a shell".to_owned()),
                    off: None,
                    line: Some(2),
                }],
                decided_by: vec![JudgeId::Ml, JudgeId::Diff],
                engines: Engines {
                    postdoc: crate::VERSION.to_owned(),
                    scan: "2.12.0".to_owned(),
                    cleave: "2.12.0".to_owned(),
                    isomer: "0.5.0".to_owned(),
                    traits: "abc12345".to_owned(),
                    model: "8c070d64ee9b".to_owned(),
                },
                analyzed_at: "2026-09-16T12:00:00Z".to_owned(),
                duration_ms: 4120,
            },
            cleave: reported(Severity::Benign, None, r#"{"v":"8","files":[]}"#),
            ml: reported(Severity::Hostile, Some(Level::At(25)), r#"{"prob":0.93}"#),
            llm: reported(Severity::Hostile, None, r#"{"grade":"hostile"}"#),
            diff: reported(Severity::Hostile, Some(Level::At(25)), r#"{"v":"2"}"#),
            diff_llm: Judge::Skipped {
                reason: Skipped::NotConfigured,
            },
        }
    }

    #[test]
    fn the_report_keeps_its_shape() {
        // A golden test over the whole envelope. hopper stores this and
        // beamline reads it, neither of them in Rust, so a field that quietly
        // renames or moves breaks two services that cannot be recompiled
        // against a new type. If this test changes, the schema version does.
        let json = serde_json::to_value(report()).unwrap();

        let keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "v", "sha256", "purl", "baseline", "verdict", "cleave", "ml", "llm", "diff",
                "diff_llm",
            ]
        );
        assert_eq!(json["v"], SCHEMA_VERSION);
        assert_eq!(json["verdict"]["severity"], "hostile");
        assert_eq!(json["verdict"]["fires_at"], 25);
        assert_eq!(json["baseline"]["fires_at"], -1);
        assert_eq!(json["diff_llm"]["status"], "skipped");
    }

    #[test]
    fn every_judge_key_is_present_whatever_happened() {
        // The rule a consumer relies on: read `status`, branch there, never
        // test whether the key exists.
        let mut only_ml = report();
        only_ml.cleave = Judge::Skipped {
            reason: Skipped::NotConfigured,
        };
        only_ml.llm = Judge::Error {
            reason: "endpoint timed out".to_owned(),
            duration_ms: 60_000,
        };
        only_ml.diff = Judge::Skipped {
            reason: Skipped::NoBaseline,
        };

        let json = serde_json::to_value(&only_ml).unwrap();
        for id in JudgeId::ALL {
            let judge = json.get(id.as_str()).unwrap_or_else(|| {
                panic!("judge {id} must be present in every report");
            });
            assert!(judge["status"].is_string(), "{id} must say what happened");
        }
        assert_eq!(json["llm"]["duration_ms"], 60_000);
        assert_eq!(json["diff"]["reason"], "no_baseline");
    }

    #[test]
    fn a_report_survives_the_round_trip() {
        let original = report();
        let encoded = serde_json::to_string(&original).unwrap();
        let decoded: Report = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            serde_json::to_string(&decoded).unwrap(),
            encoded,
            "re-encoding a decoded report must reproduce it byte for byte"
        );
    }

    #[test]
    fn stripping_evidence_leaves_every_verdict_intact() {
        let mut stripped = report();
        stripped.strip_raw();

        let json = serde_json::to_value(&stripped).unwrap();
        assert_eq!(json["verdict"]["severity"], "hostile");
        for (id, judge) in [
            (JudgeId::Cleave, &stripped.cleave),
            (JudgeId::Ml, &stripped.ml),
            (JudgeId::Llm, &stripped.llm),
            (JudgeId::Diff, &stripped.diff),
            (JudgeId::DiffLlm, &stripped.diff_llm),
        ] {
            if let Some(assessment) = judge.assessment() {
                assert!(assessment.raw.is_none(), "{id} kept its evidence");
            }
            let encoded = serde_json::to_value(judge).unwrap();
            assert!(encoded.get("raw").is_none(), "{id} serialized a raw key");
            assert!(encoded["status"].is_string());
        }
        // The summaries a caller gates on survive.
        assert_eq!(json["ml"]["severity"], "hostile");
        assert_eq!(json["ml"]["fires_at"], 25);
    }

    #[test]
    fn engine_output_is_carried_verbatim() {
        // postdoc must not reformat, reorder or re-encode what an engine
        // produced: a consumer comparing a judge's evidence against the same
        // engine's own output has to see the same bytes.
        let exact = r#"{"z":1,"a":[2,3],"nested":{"b":null}}"#;
        let judge = reported(Severity::Benign, None, exact);
        let encoded = serde_json::to_string(&judge).unwrap();
        assert!(
            encoded.contains(exact),
            "expected the engine's own bytes in {encoded}"
        );
    }
}
