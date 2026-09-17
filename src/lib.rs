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
//! One artifact produces one [`report::Verdict`] and, beside it, one
//! [`report::Judge`] per engine — `cleave`, `ml`, `llm`, `diff` and
//! `diff_llm`. Every judge key is present in every result: an engine that was
//! not asked says so and why, and one that failed says that instead. Each
//! judge carries its own engine's native output, so the evidence sits with
//! the opinion it supports rather than in a shared pile.

pub mod combine;
pub mod engine;
pub mod judge;
pub mod report;
pub mod result;

pub use combine::Outcome;
pub use judge::{Analysis, report};
pub use report::{Assessment, Engines, Finding, Judge, JudgeId, Level, Severity, Skipped, Verdict};
pub use result::{Baseline, EngineJudge, Report, SCHEMA_VERSION};

/// This crate's version, as recorded in [`Engines::postdoc`].
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
