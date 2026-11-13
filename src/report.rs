//! The two scales every engine's opinion is read on: a band ([`Severity`])
//! and the false-positive budget that encodes it ([`Level`]).
//!
//! What postdoc uploads is scan's version 8 result body (scan's
//! `JSON_PLAN.md`), with isomer's opinions added by [`crate::judge`]. These
//! types are what that fold is computed in, free of engine types so it can be
//! reviewed and tested without an analysis stack.

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
