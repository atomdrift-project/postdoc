//! Worker and server coordination for the Atomdrift analysis fleet.
//!
//! postdoc claims artifacts from hopper, asks each analysis engine what it
//! makes of one, and reports the answers together with the verdict they fold
//! into. It replaces `atomscan serve` and `atomscan worker`, and adds one
//! thing neither had: when an artifact names a package, hopper can point at an
//! earlier release of that package, and isomer judges the difference. A
//! release that is clean on its own can still have added something.
//!
//! # postdoc grades nothing
//!
//! Every severity in a result belongs to the engine that produced it. The
//! trait floor is scan's rule. The firing level is the azoth model's,
//! read through scan. The per-file interpreter's authority, including whether
//! it may soften a verdict, is scan's. The differential grade, its mapping
//! into bands, and the diff interpreter's authority are isomer's. postdoc
//! owns two things: the order the engines are asked in, and keeping the worse
//! of the two finished outcomes. It holds no thresholds, no trait lists and
//! no prompts, and it adds no opinion of its own to what the engines say.
//!
//! That constraint is what makes the result worth storing. A verdict here can
//! be traced to an engine and a build, and re-derived by running that engine
//! again.
//!
//! # Shape of a result
//!
//! A result is scan's: the version 8 body it uploads to hopper, with its
//! verdict on the artifact and everything inside it, and its own `ml`,
//! `floor` and `llm` jurors (scan's `docs/JSON.md`). When isomer compared the
//! artifact against an earlier release, its two opinions join them as the
//! `drift_heuristic` and `drift_llm` jurors, and the verdict becomes the worse of
//! scan's and isomer's, naming the juror that decided it ([`judge`]).
//! Third-party scanners (ClamAV, malcontent) report beside the jurors under
//! `scanners`, and never move the verdict ([`scanners`]).

pub mod combine;
pub mod engine;
pub mod judge;
pub mod report;
pub mod result;
pub mod scanners;

pub use combine::Outcome;
pub use judge::{Analysis, judge};
pub use report::{Level, Severity};
pub use result::Baseline;

/// The longest prefix of `text` within `max` bytes that ends on a character
/// boundary.
fn bounded(text: &str, max: usize) -> &str {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_cut_on_a_character_boundary() {
        assert_eq!(bounded("aé", 2), "a");
        assert_eq!(bounded("short", 400), "short");
        assert!(bounded(&"é".repeat(400), 400).len() <= 400);
    }
}
