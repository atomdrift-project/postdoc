//! Folding the engines' outcomes into one verdict.
//!
//! This is the whole of postdoc's judgement, and it is deliberately thin.
//! Two engines each hand over one finished outcome:
//!
//! * **scan** has already taken the azoth decision, raised it by its trait
//!   floor, and blended in its interpreter under its own bound.
//! * **isomer** has already graded the differential and applied its own
//!   interpreter under its own rules.
//!
//! postdoc keeps the worse of the two. It does not re-weigh the pieces, does
//! not bound an interpreter, and has no thresholds of its own — every band
//! here was decided by the engine it came from. The per-judge objects in a
//! result exist so a reader can see those pieces, not so postdoc can score
//! them.

use crate::report::{JudgeId, Level, Severity};

/// One engine's finished opinion.
///
/// A band and, where the engine measures one, the budget that encodes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// The band the engine settled on.
    pub severity: Severity,
    /// The budget at which it grades hostile, when a calibrated one applies.
    pub fires_at: Option<Level>,
}

impl Outcome {
    /// The worse of two outcomes.
    ///
    /// The more severe band wins outright, carrying its own budget. Between
    /// two outcomes in the same band the tighter budget wins, because two
    /// engines agreeing on the band still disagree about how narrowly the
    /// artifact escapes conviction, and the narrower reading is the one a
    /// caller should get. A measured budget beats no budget at all.
    #[must_use]
    pub fn worse_of(self, other: Self) -> Self {
        match self.severity.cmp(&other.severity) {
            std::cmp::Ordering::Greater => self,
            std::cmp::Ordering::Less => other,
            std::cmp::Ordering::Equal => Self {
                severity: self.severity,
                fires_at: match (self.fires_at, other.fires_at) {
                    (Some(a), Some(b)) => Some(a.worse_of(b)),
                    (Some(level), None) | (None, Some(level)) => Some(level),
                    (None, None) => None,
                },
            },
        }
    }
}

/// The worst outcome any engine reported, or `None` if none did.
///
/// `None` is not "benign": it is "nothing judged this", which a caller turns
/// into an unanalyzed answer rather than a clean one.
#[must_use]
pub fn worst(outcomes: impl IntoIterator<Item = Outcome>) -> Option<Outcome> {
    outcomes.into_iter().reduce(Outcome::worse_of)
}

/// The judges whose own band matches the verdict.
///
/// Answers "who decided this", so a reader can tell a model verdict from a
/// trait-floor verdict from a differential one without re-deriving the fold.
/// Judges that did not report are not credited. Order follows
/// [`JudgeId::ALL`] when the caller supplies them in that order, which keeps
/// the field stable across runs.
#[must_use]
pub fn decided_by(
    severity: Severity,
    judges: impl IntoIterator<Item = (JudgeId, Option<Severity>)>,
) -> Vec<JudgeId> {
    judges
        .into_iter()
        .filter(|&(_, reported)| reported == Some(severity))
        .map(|(id, _)| id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn unlevelled(severity: Severity) -> Outcome {
        Outcome {
            severity,
            fires_at: None,
        }
    }

    #[test]
    fn the_more_severe_band_wins_and_keeps_its_own_budget() {
        let benign = clean();
        let hostile = at(Severity::Hostile, 25);
        assert_eq!(benign.worse_of(hostile), hostile);
        assert_eq!(hostile.worse_of(benign), hostile);
    }

    #[test]
    fn a_tie_takes_the_tighter_budget() {
        let strict = at(Severity::Hostile, 10);
        let loose = at(Severity::Hostile, 25);
        assert_eq!(strict.worse_of(loose), strict);
        assert_eq!(loose.worse_of(strict), strict);
    }

    #[test]
    fn a_tie_prefers_a_measured_budget_over_none() {
        let measured = at(Severity::Suspicious, 3000);
        let manual = unlevelled(Severity::Suspicious);
        assert_eq!(measured.worse_of(manual), measured);
        assert_eq!(manual.worse_of(measured), measured);
    }

    #[test]
    fn a_tie_with_no_budget_either_side_stays_unlevelled() {
        let manual = unlevelled(Severity::Hostile);
        assert_eq!(manual.worse_of(manual), manual);
    }

    #[test]
    fn two_clean_reads_stay_clean() {
        assert_eq!(clean().worse_of(clean()), clean());
    }

    #[test]
    fn worse_of_is_commutative_and_idempotent() {
        let cases = [
            clean(),
            at(Severity::Benign, 3000),
            at(Severity::Suspicious, 3000),
            at(Severity::Suspicious, 100),
            at(Severity::Hostile, 25),
            at(Severity::Hostile, 0),
            unlevelled(Severity::Benign),
            unlevelled(Severity::Hostile),
        ];
        for a in cases {
            assert_eq!(a.worse_of(a), a, "idempotent on {a:?}");
            for b in cases {
                assert_eq!(
                    a.worse_of(b),
                    b.worse_of(a),
                    "a verdict must not depend on which engine finished first"
                );
            }
        }
    }

    #[test]
    fn worse_of_is_associative() {
        // `worst` reduces left to right; an operator that is not associative
        // would make the verdict depend on the order judges are listed in.
        let cases = [
            clean(),
            at(Severity::Suspicious, 3000),
            at(Severity::Hostile, 25),
            unlevelled(Severity::Suspicious),
        ];
        for a in cases {
            for b in cases {
                for c in cases {
                    assert_eq!(
                        a.worse_of(b).worse_of(c),
                        a.worse_of(b.worse_of(c)),
                        "({a:?} . {b:?}) . {c:?} must equal {a:?} . ({b:?} . {c:?})"
                    );
                }
            }
        }
    }

    #[test]
    fn the_fold_never_invents_an_outcome() {
        // postdoc grades nothing: what it returns is always, exactly, one of
        // the outcomes an engine handed it. If this ever fails, postdoc has
        // started synthesizing a verdict no engine reported and became a judge
        // of its own — the one thing this crate must not be.
        let cases = [
            clean(),
            at(Severity::Benign, 3000),
            at(Severity::Suspicious, 3000),
            at(Severity::Suspicious, 100),
            at(Severity::Hostile, 25),
            at(Severity::Hostile, 0),
            unlevelled(Severity::Benign),
            unlevelled(Severity::Suspicious),
            unlevelled(Severity::Hostile),
        ];
        for a in cases {
            for b in cases {
                let folded = a.worse_of(b);
                assert!(
                    folded == a || folded == b,
                    "{a:?} folded with {b:?} produced {folded:?}, which is neither"
                );
            }
        }
    }

    #[test]
    fn nothing_judged_is_not_a_clean_bill() {
        assert_eq!(worst([]), None);
    }

    #[test]
    fn one_engine_alone_decides() {
        let only = at(Severity::Suspicious, 3000);
        assert_eq!(worst([only]), Some(only));
    }

    #[test]
    fn scan_and_isomer_fold_to_the_worse_one() {
        // The case the differential exists for: scan sees a clean file, the
        // diff against the previous release does not.
        let scan = clean();
        let isomer = at(Severity::Hostile, 25);
        assert_eq!(worst([scan, isomer]), Some(isomer));

        // And the reverse: a hostile file whose release added nothing.
        let scan = at(Severity::Hostile, 25);
        let isomer = clean();
        assert_eq!(worst([scan, isomer]), Some(scan));
    }

    #[test]
    fn a_missing_baseline_leaves_scans_own_verdict_untouched() {
        // No isomer outcome at all is the common case; it must not dilute or
        // raise what scan concluded on its own.
        let scan = at(Severity::Suspicious, 3000);
        assert_eq!(worst([scan].into_iter().chain(None)), Some(scan));
    }

    #[test]
    fn credit_goes_to_every_judge_that_reached_the_verdict() {
        let by = decided_by(
            Severity::Hostile,
            [
                (JudgeId::Cleave, Some(Severity::Hostile)),
                (JudgeId::Ml, Some(Severity::Benign)),
                (JudgeId::Llm, Some(Severity::Hostile)),
                (JudgeId::Diff, None),
                (JudgeId::DiffLlm, None),
            ],
        );
        assert_eq!(by, vec![JudgeId::Cleave, JudgeId::Llm]);
    }

    #[test]
    fn a_judge_that_never_reported_is_never_credited() {
        // `None` is "did not report". It must not match a benign verdict just
        // because both are the absence of an alarm.
        let by = decided_by(
            Severity::Benign,
            [
                (JudgeId::Ml, Some(Severity::Benign)),
                (JudgeId::Diff, None),
                (JudgeId::DiffLlm, None),
            ],
        );
        assert_eq!(by, vec![JudgeId::Ml]);
    }

    #[test]
    fn credit_keeps_the_order_it_was_given() {
        let by = decided_by(
            Severity::Hostile,
            JudgeId::ALL.map(|id| (id, Some(Severity::Hostile))),
        );
        assert_eq!(by, JudgeId::ALL.to_vec(), "stable across runs");
    }

    #[test]
    fn credit_can_be_empty_only_when_nothing_matched() {
        // Defensive: the caller derives `severity` from these same judges, so
        // an empty list means the caller passed a band no judge reported.
        let by = decided_by(Severity::Hostile, [(JudgeId::Ml, Some(Severity::Benign))]);
        assert!(by.is_empty());
    }
}
