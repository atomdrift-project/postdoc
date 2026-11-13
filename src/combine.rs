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
//! here was decided by the engine it came from. Each engine's own opinions
//! travel beside the verdict as jurors, so a reader can see those pieces, not
//! so postdoc can score them.

use crate::report::{Level, Severity};

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

/// The verdict: what scan concluded, raised by what isomer concluded.
///
/// Scan always reports, because there is always a file to judge. Isomer
/// reports only when hopper pointed at an earlier release. That is the whole
/// truth about how many outcomes exist, so this takes those two rather than an
/// iterator — an iterator could be empty, and "no engine judged this" is a
/// state that cannot arise and must not need handling.
#[must_use]
pub fn verdict(scan: Outcome, isomer: Option<Outcome>) -> Outcome {
    isomer.map_or(scan, |isomer| scan.worse_of(isomer))
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
        // Not load-bearing for two outcomes, but it is what lets a third
        // engine be folded in later without the verdict depending on where in
        // the order it lands.
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
    fn scan_and_isomer_fold_to_the_worse_one() {
        // The case the differential exists for: scan sees a clean file, the
        // diff against the previous release does not.
        let scan = clean();
        let isomer = at(Severity::Hostile, 25);
        assert_eq!(verdict(scan, Some(isomer)), isomer);

        // And the reverse: a hostile file whose release added nothing.
        let scan = at(Severity::Hostile, 25);
        let isomer = clean();
        assert_eq!(verdict(scan, Some(isomer)), scan);
    }

    #[test]
    fn a_missing_baseline_leaves_scans_own_verdict_untouched() {
        // No isomer outcome at all is the common case; it must not dilute or
        // raise what scan concluded on its own.
        let scan = at(Severity::Suspicious, 3000);
        assert_eq!(verdict(scan, None), scan);
    }
}
