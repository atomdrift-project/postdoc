//! What an analysis was compared against.

use serde::{Deserialize, Serialize};

use crate::report::Level;

/// The earlier release an artifact was compared against.
///
/// An input, not a finding: the drift jurors name it by digest (`base`).
/// Hopper picks it, because hopper is the only party that knows what it
/// holds.
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
