//! The two operational routes: `/_/health` and `/_/stats`.
//!
//! `/_/health` is the one unauthenticated route, so it says one word and
//! nothing else. `/_/stats` is beamline's routing signal, behind the token.
//! Its body is a typed struct whose field order is the wire order, and
//! beamline indexes it by name, so `stats_keys_are_pinned` holds the names in
//! place.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use super::{AppState, JobBucket, PURL_TYPE_NAMES, SIZE_BUCKET_NAMES};

const MIB: u64 = 1024 * 1024;

fn rss_mb() -> Option<u64> {
    cleave::memory_tracker::current_rss().map(|b| b / MIB)
}

/// GET /_/health — whether this server can take work, in one word.
///
/// `ok` (200) while serving, including when every slot is busy: a full pool is
/// the target steady state, and the analyze routes refuse past capacity on
/// their own. `starting`, `failed` and `degraded` (503) say why it cannot.
///
/// Reachable without a token, so it reports readiness and nothing else — no
/// load, memory, versions or sample names. Everything an operator or router
/// needs beyond that is on `/_/stats`, behind the token.
pub(super) async fn health(State(state): State<Arc<AppState>>) -> Response {
    let word = if let Some(message) = state.init_failure() {
        tracing::error!("GET /_/health -> 503 (failed: {message})");
        "failed"
    } else if !state.is_ready() {
        tracing::debug!("GET /_/health -> 503 (starting)");
        "starting"
    } else if let Some((rss, max)) = cleave::memory_tracker::current_rss()
        .zip(state.config.max_rss_bytes)
        .filter(|&(rss, max)| rss > max.get())
    {
        tracing::warn!(
            rss_mb = rss / MIB,
            max_rss_mb = max.get() / MIB,
            "GET /_/health -> 503 (degraded: memory pressure)"
        );
        "degraded"
    } else {
        return (StatusCode::OK, "ok\n").into_response();
    };
    (StatusCode::SERVICE_UNAVAILABLE, format!("{word}\n")).into_response()
}

/// A class of work's windowed latency, in milliseconds.
#[derive(Serialize)]
pub(crate) struct Recent {
    pub(crate) samples: u64,
    pub(crate) p80_ms: Option<u64>,
    pub(crate) mean_ms: Option<u64>,
}

/// One class of work on `/_/stats`.
#[derive(Serialize)]
pub(super) struct BucketStats {
    jobs: u64,
    avg_ms: Option<u64>,
    /// The one a router should read: a p80 over the last hour rather than a
    /// mean over the last few hundred jobs.
    recent: Recent,
}

/// Named buckets, as a JSON object keyed by name.
pub(super) struct Buckets<'a> {
    names: &'a [&'static str],
    buckets: &'a [JobBucket],
}

impl Serialize for Buckets<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.names.iter().zip(self.buckets).map(|(name, b)| {
            let (jobs, avg_ms) = b.mean_ms();
            (
                *name,
                BucketStats {
                    jobs,
                    avg_ms,
                    recent: b.recent(),
                },
            )
        }))
    }
}

#[derive(Serialize)]
pub(super) struct WhaleSlots {
    in_use: usize,
    max: usize,
}

#[derive(Serialize)]
pub(super) struct Uploads {
    pending: usize,
    capacity: usize,
    failed: usize,
    uploaded: usize,
}

#[derive(Serialize)]
pub(super) struct StatsIdleWorker {
    running: bool,
    frozen: bool,
}

/// `/_/stats`. The comment on each field says what a router reads it for.
#[derive(Serialize)]
pub(super) struct Stats<'a> {
    /// Where lookups this index could not answer actually went. A fleet
    /// quietly reading from the primary because the replica stopped answering
    /// is otherwise indistinguishable from one reading the replica, until the
    /// primary's load says so.
    corpus: Option<super::corpus::CorpusStats>,
    /// Saturation. The best routing signal available, because it is current
    /// rather than lagging: a latency average still reports health for the
    /// minute after a server takes on four large archives.
    slots: usize,
    slots_free: usize,
    in_flight: usize,
    /// Cores of this box's load that will not be there when a request
    /// arrives, for a router to subtract before calling the box saturated.
    /// Without it, load alone made a server full of sheddable work look
    /// identical to one full of requests (2026-09-05). See beamline's
    /// `foregroundPressure`.
    background_in_flight: usize,
    /// The basis `slots` was sized on, so a caller can read `load1` against
    /// the right denominator. Worth reporting because `in_flight` is this
    /// process's own count and nothing else's: measured on a 64-core node,
    /// `slots_free=64 in_flight=0` alongside `load1=50`.
    physical_cpus: Option<usize>,
    /// Logical cores the whole machine kept busy since the previous poll, from
    /// the kernel's CPU counters. Preferred over `load1` because it means the
    /// same thing on every platform. `null` until two polls exist or where the
    /// platform has no counters.
    cpu_busy_cores: Option<f64>,
    /// Big-whale slots: the one wait on the interactive path that is not a
    /// core or a slot, and the one that held two requests for their whole
    /// budget on 2026-09-06.
    whale_slots: WhaleSlots,
    overloaded: bool,
    stuck_orphans: usize,
    /// What this server has actually done. More useful for routing than a
    /// load average, which is a whole-host number that folds in every other
    /// tenant on the box.
    uptime_secs: u64,
    jobs_started: u64,
    jobs_completed: u64,
    /// Begun and never finished: timeouts, panics, and clients that hung up
    /// mid-analysis. Non-zero and climbing is the shape of a sick server.
    jobs_unfinished: u64,
    avg_job_bytes: Option<u64>,
    /// Fresh analyses only: what this server costs on work it has not seen.
    avg_job_ms: Option<u64>,
    /// Answered from cleave's analysis cache, kept out of the figures above
    /// so it cannot flatter a server into looking fast at work it never did.
    avg_job_ms_cached: Option<u64>,
    cached_samples: u64,
    /// Windowed p80, for the fleet-wide view, beside the lifetime means.
    recent: Recent,
    recent_lookup: Recent,
    latency_window_secs: u64,
    /// `/lookup` service time — an index probe, near-constant in the size of
    /// the artifact, and three orders of magnitude below an analysis.
    avg_lookup_ms: Option<u64>,
    /// Microseconds too: a healthy index probe rounds to 0ms, and a routing
    /// signal that is always zero is no signal at all.
    avg_lookup_us: Option<u64>,
    lookup_samples: u64,
    /// Lifetime, for the operator rather than the router.
    avg_job_ms_lifetime: Option<u64>,
    avg_job_samples: u64,
    /// By PURL type: a `?purl=` request has no size until the artifact is
    /// fetched, so this is what a router can compare on when the choice still
    /// matters.
    avg_job_ms_by_type: Buckets<'a>,
    /// By input size: a caller that knows how big the artifact is should
    /// compare servers at that size, not overall.
    avg_job_ms_by_size: Buckets<'a>,
    /// Memory headroom. A server near its ceiling is about to pause admission;
    /// a router should move away before that, not discover it by timing out.
    rss_mb: Option<u64>,
    max_rss_mb: Option<u64>,
    load1: Option<f64>,
    /// Capability, not speed. A server without 7z cannot read a DMG, so it
    /// returns a *weaker verdict* rather than a slower one.
    tools: Vec<&'static str>,
    max_upload_mb: usize,
    /// Verdict comparability. A scanner on stale traits produces an answer
    /// that should not be cached as authoritative alongside a current one.
    traits_commit: Option<String>,
    model_commit: Option<String>,
    ready: bool,
    /// Whether the verdicts it computes are actually reaching hopper.
    /// `failed` climbing means work is being done and then lost.
    uploads: Option<Uploads>,
    /// Whether spare capacity is genuinely spare.
    idle_worker: StatsIdleWorker,
}

impl<'a> Stats<'a> {
    /// Blocking: CPU counters, memory, tools and commits are read from the
    /// system and from disk.
    fn collect(state: &'a AppState) -> Self {
        let jobs = &state.jobs;
        let in_flight = state.in_flight.len();
        let started = jobs.started.load(Ordering::Relaxed);
        let completed = jobs.completed.load(Ordering::Relaxed);
        // Averages over completed jobs only: a job still running has
        // contributed no duration, and dividing by `started` would report
        // every busy server as faster than it is.
        let avg = |total: &std::sync::atomic::AtomicU64| {
            (completed > 0).then(|| total.load(Ordering::Relaxed) / completed)
        };
        let (recent_n, avg_job_ms) = jobs.overall.mean_ms();
        let (cached_n, avg_job_ms_cached) = jobs.cached.mean_ms();
        let lookup_n = jobs.lookups.count.load(Ordering::Relaxed);
        let lookup_micros = jobs.lookups.micros.load(Ordering::Relaxed);
        let (whale_in_use, whale_max) = scan::analysis::whale_slot_usage();
        Self {
            corpus: state.corpus.as_ref().map(|c| c.stats()),
            slots: state.config.workers,
            slots_free: state.available_analysis_permits(),
            in_flight,
            background_in_flight: state.sheddable_cores(),
            physical_cpus: cleave::memory_tracker::physical_cpu_count(),
            cpu_busy_cores: state.cpu_busy.sample(),
            whale_slots: WhaleSlots {
                in_use: whale_in_use,
                max: whale_max,
            },
            overloaded: state.overload_mark().is_some(),
            stuck_orphans: state.stuck_orphans.load(Ordering::Relaxed),
            uptime_secs: state.started_at.elapsed().as_secs(),
            jobs_started: started,
            jobs_completed: completed,
            jobs_unfinished: started
                .saturating_sub(completed)
                .saturating_sub(in_flight as u64),
            avg_job_bytes: avg(&jobs.bytes_total),
            avg_job_ms,
            avg_job_ms_cached,
            cached_samples: cached_n,
            recent: jobs.overall.recent(),
            recent_lookup: jobs.lookups.recent(),
            latency_window_secs: super::latency::WINDOW.as_secs(),
            avg_lookup_ms: (lookup_n > 0).then(|| lookup_micros / lookup_n / 1_000),
            avg_lookup_us: (lookup_n > 0).then(|| lookup_micros / lookup_n),
            lookup_samples: lookup_n,
            avg_job_ms_lifetime: avg(&jobs.micros_total).map(|us| us / 1_000),
            avg_job_samples: recent_n,
            avg_job_ms_by_type: Buckets {
                names: &PURL_TYPE_NAMES,
                buckets: &jobs.by_type,
            },
            avg_job_ms_by_size: Buckets {
                names: &SIZE_BUCKET_NAMES,
                buckets: &jobs.by_size,
            },
            rss_mb: rss_mb(),
            max_rss_mb: state.config.max_rss_bytes.map(|n| n.get() / MIB),
            load1: scan::system_load_avg(),
            tools: scan::tools::available_names(),
            max_upload_mb: state.config.max_body_size / 1024 / 1024,
            traits_commit: cleave::traits_repo::version(),
            model_commit: scan::models_repo::version(),
            ready: state.is_ready(),
            uploads: state.uploader.as_ref().map(|u| {
                let s = u.stats();
                Uploads {
                    pending: s.pending,
                    capacity: s.capacity,
                    failed: s.failed,
                    uploaded: s.uploaded,
                }
            }),
            idle_worker: StatsIdleWorker {
                running: state.idle_worker_running(),
                frozen: state.is_busy(),
            },
        }
    }
}

/// `GET /_/stats` — the live signals a router needs to choose this server.
/// Everything here moves every second and must not be cached.
///
/// The vocabulary deliberately matches what the pull worker already advertises
/// to hopper on `/v1/work/next` — slots, rss, load, max_bytes, traits, tools — so a
/// caller reasoning about which scanner to use is reading the same facts hopper
/// uses to ration work, rather than a second dialect of the same idea.
pub(super) async fn stats(State(state): State<Arc<AppState>>) -> Response {
    // Serialized on the blocking thread too: `Stats` borrows the state.
    match tokio::task::spawn_blocking(move || serde_json::to_vec(&Stats::collect(&state))).await {
        Ok(Ok(body)) => (
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            body,
        )
            .into_response(),
        Ok(Err(e)) => {
            tracing::error!(error = %e, "could not serialize /_/stats");
            super::error::ApiError::internal().into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "collecting /_/stats panicked");
            super::error::ApiError::internal().into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::ServerConfig;

    /// The keys of a serialized body, in wire order.
    fn keys<T: Serialize>(body: &T) -> Vec<String> {
        serde_json::to_value(body)
            .expect("serializes")
            .as_object()
            .expect("an object")
            .keys()
            .cloned()
            .collect()
    }

    /// Beamline indexes these bodies by name, so a key that moves or is
    /// renamed is an outage that looks like nothing.
    #[tokio::test]
    async fn stats_keys_are_pinned() {
        let (_app, state) = super::super::assemble(ServerConfig {
            model_dir: std::env::temp_dir(),
            ..ServerConfig::default()
        })
        .expect("app");

        assert_eq!(
            keys(&Stats::collect(&state)),
            [
                "corpus",
                "slots",
                "slots_free",
                "in_flight",
                "background_in_flight",
                "physical_cpus",
                "cpu_busy_cores",
                "whale_slots",
                "overloaded",
                "stuck_orphans",
                "uptime_secs",
                "jobs_started",
                "jobs_completed",
                "jobs_unfinished",
                "avg_job_bytes",
                "avg_job_ms",
                "avg_job_ms_cached",
                "cached_samples",
                "recent",
                "recent_lookup",
                "latency_window_secs",
                "avg_lookup_ms",
                "avg_lookup_us",
                "lookup_samples",
                "avg_job_ms_lifetime",
                "avg_job_samples",
                "avg_job_ms_by_type",
                "avg_job_ms_by_size",
                "rss_mb",
                "max_rss_mb",
                "load1",
                "tools",
                "max_upload_mb",
                "traits_commit",
                "model_commit",
                "ready",
                "uploads",
                "idle_worker",
            ]
        );
        let stats = serde_json::to_value(Stats::collect(&state)).expect("serializes");
        assert_eq!(
            stats["avg_job_ms_by_type"]
                .as_object()
                .expect("by type")
                .keys()
                .collect::<Vec<_>>(),
            ["cargo", "golang", "npm", "pypi", "other"]
        );
        assert_eq!(
            keys(&stats["avg_job_ms_by_size"]["le_1mb"]),
            ["jobs", "avg_ms", "recent"]
        );
        assert_eq!(keys(&stats["recent"]), ["samples", "p80_ms", "mean_ms"]);
        assert_eq!(keys(&stats["whale_slots"]), ["in_use", "max"]);
        assert_eq!(keys(&stats["idle_worker"]), ["running", "frozen"]);

        let running = super::super::handlers::StatusBody {
            state: "running",
            purl: None,
            url: None,
            sha256: Some("a".repeat(64)),
            elapsed_ms: Some(5),
            attached: Some(1),
        };
        assert_eq!(
            keys(&running),
            ["state", "purl", "url", "sha256", "elapsed_ms", "attached"]
        );
        let unknown = super::super::handlers::StatusBody {
            state: "unknown",
            elapsed_ms: None,
            attached: None,
            ..running
        };
        assert_eq!(keys(&unknown), ["state", "purl", "url", "sha256"]);
    }
}
