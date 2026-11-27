//! What the server does when its RSS stays over `--max-rss-gb`.
//!
//! It used to exit at once. That restarted the process before the kernel
//! OOM-killed it, but took every healthy analysis in flight down with the one
//! that grew — sixty callers each saw `unexpected EOF` — and logged nothing
//! about what had been running, so the culprit had to be found some other way.
//!
//! Now the overload names and stops a culprit first. The oldest analysis is
//! the likeliest: a runaway is one that has run long, and nothing else here
//! knows which analysis holds the memory. It is cancelled and its caller is
//! answered `aborted` with what it was doing; every analysis in flight is
//! logged at ERROR, with the member and rule cleave reports for it, both then
//! and before any exit. Only if RSS is still over the cap once the abort has
//! had [`ABORT_GRACE`] to free memory does the server exit, as before.

use std::fmt::Write as _;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use super::AppState;

/// How often cleave's watchdog checks RSS; it calls [`MemoryCap::overloaded`]
/// on every check once the cap has been exceeded for 30 seconds.
pub(super) const CHECK_EVERY: Duration = Duration::from_secs(10);

/// How long an aborted analysis has to unwind and free its memory before the
/// server gives up and exits.
const ABORT_GRACE: Duration = Duration::from_secs(30);

/// A pause between calls longer than this means RSS dipped under the cap in
/// between: the next call starts a new episode, with an abort of its own.
const EPISODE_GAP: Duration = Duration::from_secs(3 * CHECK_EVERY.as_secs());

/// Longest member path quoted in an abort message.
const MAX_MEMBER_CHARS: usize = 200;

/// The escalation state of one overload episode.
#[derive(Debug)]
pub(super) struct MemoryCap {
    state: Weak<AppState>,
    last_call: Option<Instant>,
    aborted_at: Option<Instant>,
}

impl MemoryCap {
    pub(super) fn new(state: &Arc<AppState>) -> Self {
        Self {
            state: Arc::downgrade(state),
            last_call: None,
            aborted_at: None,
        }
    }

    /// RSS is still over the cap: abort the oldest analysis on the first call
    /// of an episode, then exit if the overload outlasts the grace period.
    pub(super) fn overloaded(&mut self) {
        let now = Instant::now();
        if self
            .last_call
            .is_none_or(|last| now.duration_since(last) > EPISODE_GAP)
        {
            self.aborted_at = None;
        }
        self.last_call = Some(now);
        let Some(state) = self.state.upgrade() else {
            exit();
        };
        match self.aborted_at {
            None => {
                census(
                    &state,
                    "memory cap: RSS over --max-rss-gb for >30s; aborting the oldest analysis",
                );
                abort_oldest(&state);
                self.aborted_at = Some(now);
            }
            Some(at) if now.duration_since(at) < ABORT_GRACE => {}
            Some(at) => {
                census(
                    &state,
                    "memory cap: RSS still over --max-rss-gb after aborting; exiting",
                );
                tracing::error!(
                    since_abort_secs = now.duration_since(at).as_secs(),
                    cap_aborts = state.cap_aborts.load(Ordering::Relaxed),
                    "memory cap exit: RSS over --max-rss-gb, exiting to avoid an OOM kill",
                );
                exit();
            }
        }
    }
}

fn exit() -> ! {
    std::process::exit(1);
}

/// Log every analysis in flight at ERROR, oldest first: what was running when
/// memory ran out, so the culprit is named even if the process dies next.
fn census(state: &AppState, why: &str) {
    let mut rows: Vec<_> = state.in_flight.iter().collect();
    rows.sort_by_key(|entry| entry.started_at);
    tracing::error!(
        in_flight = rows.len(),
        rss_mb = cleave::memory_tracker::current_rss().map(|rss| rss >> 20),
        limit_mb = state.config.max_rss_bytes.map(|limit| limit.get() >> 20),
        "{why}",
    );
    for entry in rows {
        let progress = entry.progress();
        tracing::error!(
            request_id = *entry.key(),
            name = %entry.name,
            sha256 = entry.sha256.as_deref(),
            elapsed_secs = entry.started_at.elapsed().as_secs(),
            phase = entry.phase.current(),
            member = progress.member.as_ref().map(|s| s.name.as_str()),
            member_secs = progress.member.as_ref().map(|s| s.age.as_secs()),
            "trait" = progress.rule.as_ref().map(|s| s.name.as_str()),
            trait_secs = progress.rule.as_ref().map(|s| s.age.as_secs()),
            cancelled = entry.cancellation.load(Ordering::Acquire),
            "memory cap census: in flight",
        );
    }
}

/// Cancel the oldest analysis not already stopping, and answer its caller
/// with why. One already cancelled (a timed-out orphan) is skipped: raising
/// its flag again would free nothing.
fn abort_oldest(state: &AppState) {
    let oldest = state
        .in_flight
        .iter()
        .filter(|entry| !entry.cancellation.load(Ordering::Acquire))
        .min_by_key(|entry| entry.started_at);
    let Some(entry) = oldest else {
        tracing::error!("memory cap: no running analysis left to abort");
        return;
    };
    let phase = entry.phase.current();
    let progress = entry.progress();
    let member = progress.member.as_ref().map(|s| s.name.as_str());
    let mut reason = format!("Analysis aborted: server memory cap exceeded (phase {phase}");
    if let Some(member) = member {
        let quoted: String = member.chars().take(MAX_MEMBER_CHARS).collect();
        let _ = write!(reason, ", member {quoted}");
    }
    reason.push(')');
    if entry.abort.raise(reason, &entry.cancellation) {
        state.cap_aborts.fetch_add(1, Ordering::Relaxed);
        tracing::error!(
            request_id = *entry.key(),
            name = %entry.name,
            sha256 = entry.sha256.as_deref(),
            elapsed_secs = entry.started_at.elapsed().as_secs(),
            phase,
            member,
            "trait" = progress.rule.as_ref().map(|s| s.name.as_str()),
            "memory cap: aborted the oldest analysis",
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use scan::analysis::RequestPhase;
    use tokio::sync::Semaphore;

    use super::super::{
        AnalysisOutcome, AnalysisPermit, InFlightRequest, RequestGuard, ServerConfig, assemble,
    };
    use super::*;

    async fn permit() -> anyhow::Result<AnalysisPermit> {
        Ok(AnalysisPermit::new(
            Arc::new(Semaphore::new(1)).acquire_owned().await?,
            Arc::new(Semaphore::new(1)).acquire_owned().await?,
        ))
    }

    /// The oldest analysis is the one stopped, its caller is answered at once
    /// with why, and the abort is counted; a younger one keeps running.
    #[tokio::test]
    async fn the_oldest_analysis_is_aborted_and_answered_with_why() -> anyhow::Result<()> {
        let (_router, state) = assemble(ServerConfig::default())?;
        let oldest = Arc::new(AtomicBool::new(false));
        let entry = InFlightRequest::new(
            "huge.tgz",
            Some(&"ab".repeat(32)),
            Arc::clone(&oldest),
            RequestPhase::with_label("req#1 huge.tgz"),
        );
        entry.phase.set("cleave:analyze");
        let guard = RequestGuard::new(&state, 1, entry, permit().await?);
        let flag = Arc::clone(&oldest);
        let run = tokio::spawn(guard.run(move || {
            // An analysis that only stops when asked to.
            while !flag.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(5));
            }
            anyhow::bail!("analysis cancelled")
        }));

        let younger = Arc::new(AtomicBool::new(false));
        let entry = InFlightRequest::new(
            "small.tgz",
            None,
            Arc::clone(&younger),
            RequestPhase::with_label("req#2 small.tgz"),
        );
        let _young = RequestGuard::new(&state, 2, entry, permit().await?);

        abort_oldest(&state);
        let outcome = tokio::time::timeout(Duration::from_secs(10), run).await??;
        match outcome {
            AnalysisOutcome::Aborted(reason) => {
                assert!(reason.contains("memory cap"), "{reason}");
                assert!(reason.contains("cleave:analyze"), "{reason}");
            }
            other => panic!("expected an abort, got {other:?}"),
        }
        assert!(oldest.load(Ordering::Acquire));
        assert!(
            !younger.load(Ordering::Acquire),
            "only the oldest is stopped"
        );
        assert_eq!(state.cap_aborts.load(Ordering::Relaxed), 1);

        // A second abort skips the one already stopping for the next oldest.
        abort_oldest(&state);
        assert_eq!(state.cap_aborts.load(Ordering::Relaxed), 2);
        assert!(younger.load(Ordering::Acquire));
        Ok(())
    }
}
