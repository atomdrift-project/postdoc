//! The seam between the engines' vocabularies and postdoc's.
//!
//! Scan grades into three classes and a firing level; isomer grades into five
//! severities and maps them into scan's three itself. This module is where
//! those arrive, and it is the only place in postdoc that translates an
//! engine's vocabulary into postdoc's — every grade mapping, both directions,
//! on one screen. [`crate::report`] stays free of engine types so the wire
//! contract can be reviewed and tested without an analysis stack.
//!
//! Nothing here decides anything. Each conversion is a rename of a grade the
//! engine already settled on, and the one judgement call — what isomer's
//! five levels mean in three bands — lives in isomer, as
//! `isomer::Severity::band`.

use crate::report::{Level, Severity};

impl From<scan::Classification> for Severity {
    fn from(class: scan::Classification) -> Self {
        match class {
            scan::Classification::Hostile => Self::Hostile,
            scan::Classification::Suspicious => Self::Suspicious,
            // `Classification` is `#[non_exhaustive]`. A class this build has
            // not heard of is not evidence of anything, and reading it as an
            // alarm would convict artifacts on a variant nobody has defined
            // yet. It reads as benign, and the judge beside it still carries
            // the engine's own output for whoever needs the detail.
            _ => Self::Benign,
        }
    }
}

/// Scan's per-file interpreter grade, in postdoc's bands.
///
/// The same three bands under a different name, so this is a rename.
impl From<scan::interpret::LlmGrade> for Severity {
    fn from(grade: scan::interpret::LlmGrade) -> Self {
        match grade {
            scan::interpret::LlmGrade::Hostile => Self::Hostile,
            scan::interpret::LlmGrade::Suspicious => Self::Suspicious,
            scan::interpret::LlmGrade::Benign => Self::Benign,
        }
    }
}

/// A postdoc band as scan names it.
///
/// The inverse of [`Severity`]'s `From<scan::Classification>`, needed to ask
/// scan where a band's synthesized level sits. It lives beside that impl so
/// the two directions cannot drift apart; the orphan rule is why it is a
/// function and not a `From`.
#[must_use]
pub fn classification(severity: Severity) -> scan::Classification {
    match severity {
        Severity::Hostile => scan::Classification::Hostile,
        Severity::Suspicious => scan::Classification::Suspicious,
        Severity::Benign => scan::Classification::Benign,
    }
}

/// A scan firing level as postdoc records it.
///
/// Scan spells this `Option<i32>`: `None` under manual thresholds, `Some(-1)`
/// when nothing fires, and otherwise the budget. The shapes line up exactly,
/// so this is a rename and not a reinterpretation.
#[must_use]
pub fn level(scan_level: Option<i32>) -> Option<Level> {
    scan_level.map(|raw| {
        if raw < 0 {
            return Level::Never;
        }
        // Scan's calibrated grid tops out at 25000, well inside `u16`. A
        // wider value would be a different scale entirely; saturating keeps
        // it in the looser direction, which cannot manufacture a conviction.
        Level::At(u16::try_from(raw).unwrap_or(u16::MAX))
    })
}

/// What isomer concluded, in postdoc's bands.
///
/// The mapping from isomer's five grades to these three is isomer's own, so
/// that what its grades mean stays its decision and not postdoc's.
#[must_use]
pub fn isomer_severity(severity: isomer::Severity) -> Severity {
    severity.band().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grade_keeps_its_band() {
        assert_eq!(
            Severity::from(scan::interpret::LlmGrade::Hostile),
            Severity::Hostile
        );
        assert_eq!(
            Severity::from(scan::interpret::LlmGrade::Suspicious),
            Severity::Suspicious
        );
        assert_eq!(
            Severity::from(scan::interpret::LlmGrade::Benign),
            Severity::Benign
        );
    }

    #[test]
    fn a_band_survives_the_round_trip_through_scans_vocabulary() {
        // The two directions live together so they cannot drift; this is what
        // says they still agree.
        for severity in Severity::ALL {
            assert_eq!(Severity::from(classification(severity)), severity);
        }
    }

    #[test]
    fn a_scan_class_keeps_its_meaning() {
        assert_eq!(
            Severity::from(scan::Classification::Benign),
            Severity::Benign
        );
        assert_eq!(
            Severity::from(scan::Classification::Suspicious),
            Severity::Suspicious
        );
        assert_eq!(
            Severity::from(scan::Classification::Hostile),
            Severity::Hostile
        );
    }

    #[test]
    fn scans_level_encoding_reads_the_same_here() {
        assert_eq!(level(None), None, "manual thresholds carry no level");
        assert_eq!(level(Some(-1)), Some(Level::Never));
        assert_eq!(level(Some(0)), Some(Level::At(0)));
        assert_eq!(level(Some(25)), Some(Level::At(25)));
        assert_eq!(level(Some(25_000)), Some(Level::At(25_000)));
    }

    #[test]
    fn a_level_off_the_scale_cannot_manufacture_a_conviction() {
        // Unreachable from scan's grid, but a saturating read must fail
        // loose — a budget this wide convicts nobody.
        assert_eq!(level(Some(i32::MAX)), Some(Level::At(u16::MAX)));
    }

    #[test]
    fn isomers_grades_arrive_through_isomers_own_mapping() {
        // The three engines must share one dependency graph: if scan were
        // linked twice, `isomer::Severity::band` would return a
        // `scan::Classification` this crate could not accept, and this would
        // not compile. That is the point of the test as much as the values.
        assert_eq!(
            isomer_severity(isomer::Severity::Critical),
            Severity::Hostile
        );
        assert_eq!(
            isomer_severity(isomer::Severity::High),
            Severity::Suspicious
        );
        for quiet in [
            isomer::Severity::Medium,
            isomer::Severity::Low,
            isomer::Severity::None,
        ] {
            assert_eq!(
                isomer_severity(quiet),
                Severity::Benign,
                "{quiet:?} is isomer's reporting floor, not an alarm"
            );
        }
    }

    #[test]
    fn the_two_engines_agree_on_the_bands_they_share() {
        // Scan and isomer both grade into scan's classes. A verdict folded
        // from the two is only meaningful if a class means the same thing
        // whichever side it arrived from.
        assert_eq!(
            isomer_severity(isomer::Severity::Critical),
            Severity::from(scan::Classification::Hostile)
        );
        assert_eq!(
            isomer_severity(isomer::Severity::High),
            Severity::from(scan::Classification::Suspicious)
        );
        assert_eq!(
            isomer_severity(isomer::Severity::None),
            Severity::from(scan::Classification::Benign)
        );
    }
}
