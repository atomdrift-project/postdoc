//! This server's metrics, the way every atomdrift service exports them.
//!
//! `/_/stats` is a router's view: windowed averages and live saturation, read
//! by beamline to choose a worker. This is an operator's view: monotonic
//! counters that Prometheus differentiates for itself, so an alert can ask
//! what share of this server's analyses failed over the last five minutes
//! without the server keeping a window for it.
//!
//! The case that motivated it: on 2026-10-23 rdu2 lost its registry egress and
//! answered `unretrievable` for left-pad, itoa and six in 1.7s each, while its
//! `/_/stats` reported `ready` with 128 slots free. Nothing a router reads
//! could tell it apart from a healthy box; the outcome counter here can.
//!
//! The Go services get this from `obs` (`../obs`, OBSERVABILITY.md), and this
//! follows its conventions so postdoc lands beside them rather than in a
//! dialect of its own:
//!
//! - Pushed as OTLP/HTTP to the Prometheus receiver at `otel` every 15s,
//!   honouring the same `OTEL_EXPORTER_OTLP_*` overrides. Push rather than
//!   scrape because most of the fleet is not on the network Prometheus
//!   scrapes from, and a box can always reach out.
//! - Named `<service>.<noun>.<verb>`, with the same resource attributes, so the
//!   series carry `job="postdoc"` and `instance="<host>-<pid>"`.
//!
//! Hand-written rather than the OpenTelemetry SDK: every series is a counter, a
//! gauge or a cumulative histogram read from state that already exists, the
//! receiver accepts OTLP as JSON, and serde and reqwest are already here.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::flight::{FlightKey, Outcome};
use super::{AppState, PURL_TYPE_NAMES, purl_type_bucket};

/// `obs.DefaultEndpoint` plus the path the SDK appends for metrics.
const DEFAULT_ENDPOINT: &str = "http://otel:9090/api/v1/otlp/v1/metrics";

/// `obs`'s push interval.
const PUSH_EVERY: Duration = Duration::from_secs(15);

/// A push that has not finished by the next one is not going to.
const PUSH_TIMEOUT: Duration = Duration::from_secs(10);

/// Every analysis that ended, by what it was asked about and how it ended.
///
/// Counted where a flight finishes, so a run shared by ten callers counts once:
/// this measures the server's work, not its traffic. Keyed by `&'static str`
/// on both sides — an ecosystem from a fixed list and an [`ApiError`] code from
/// the source — so the series count is bounded by the code, never by callers.
///
/// A mutex rather than atomics because the outcome names are open-ended, and
/// it is taken once per finished analysis: seconds apart, not per request.
///
/// [`ApiError`]: super::error::ApiError
#[derive(Debug, Default)]
pub(super) struct Outcomes(Mutex<BTreeMap<(&'static str, &'static str), u64>>);

impl Outcomes {
    pub(super) fn record(&self, key: &FlightKey, outcome: &Outcome) {
        let ecosystem = match key {
            FlightKey::Purl(purl) | FlightKey::PurlFollow(purl, _) => {
                PURL_TYPE_NAMES[purl_type_bucket(purl)]
            }
            FlightKey::Url(_) | FlightKey::UrlFollow(_, _) => "url",
            FlightKey::Sha(_) | FlightKey::ShaFollow(_, _) => "sha256",
        };
        let ended = match outcome {
            Outcome::Report(result) if result.analysis_cached => "cached",
            Outcome::Report(_) => "analyzed",
            Outcome::Failed(refusal) => refusal.code,
        };
        let mut counts = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        *counts.entry((ecosystem, ended)).or_default() += 1;
    }

    fn snapshot(&self) -> Vec<((&'static str, &'static str), u64)> {
        let counts = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        counts.iter().map(|(k, v)| (*k, *v)).collect()
    }
}

/// Upper bounds of the phase-duration buckets, in milliseconds: from a
/// replayed cache hit to an analysis that runs into the timeout.
const PHASE_BOUNDS_MS: [u64; 14] = [
    10, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000, 60_000, 120_000, 300_000, 600_000,
];

/// One phase's cumulative duration histogram: a count per bucket of
/// [`PHASE_BOUNDS_MS`], the last for everything above them.
#[derive(Clone, Debug, Default, PartialEq)]
struct Histogram {
    buckets: [u64; PHASE_BOUNDS_MS.len() + 1],
    sum_ms: u64,
}

impl Histogram {
    fn count(&self) -> u64 {
        self.buckets.iter().sum()
    }
}

/// Where every analysis's wall time went, per phase of its timeline — the
/// completion line's `phases` field, aggregated.
///
/// Keyed by the phase names the code sets (and `pre` / `other`), so the series
/// count is bounded by the code. Parts of a phase are recorded beside it:
/// summing every phase double counts the parts of `classify:report`.
#[derive(Debug, Default)]
pub(super) struct PhaseDurations(Mutex<BTreeMap<String, Histogram>>);

impl PhaseDurations {
    pub(super) fn record(&self, timeline: &scan::analysis::Timeline) {
        let mut phases = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        for (phase, ms) in timeline.phases() {
            let bucket = PHASE_BOUNDS_MS.partition_point(|&bound| bound < ms);
            let histogram = phases.entry(phase.to_owned()).or_default();
            histogram.buckets[bucket] += 1;
            histogram.sum_ms = histogram.sum_ms.saturating_add(ms);
        }
    }

    fn snapshot(&self) -> Vec<(String, Histogram)> {
        let phases = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        phases.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Counter,
    Gauge,
    Histogram,
}

/// One data point's value.
#[derive(Debug)]
enum Point {
    Int(u64),
    Histogram(Histogram),
}

/// One metric and its current values.
#[derive(Debug)]
struct Family {
    /// The OTel name.
    name: &'static str,
    /// UCUM, as OTel spells it: `s`, `By`, or empty.
    unit: &'static str,
    kind: Kind,
    help: &'static str,
    points: Vec<(Vec<(&'static str, String)>, Point)>,
}

impl Family {
    fn new(name: &'static str, unit: &'static str, kind: Kind, help: &'static str) -> Self {
        Self {
            name,
            unit,
            kind,
            help,
            points: Vec::new(),
        }
    }

    fn gauge(name: &'static str, unit: &'static str, help: &'static str, value: u64) -> Self {
        let mut family = Self::new(name, unit, Kind::Gauge, help);
        family.points.push((Vec::new(), Point::Int(value)));
        family
    }
}

/// Everything this server reports, read now.
fn collect(state: &AppState) -> Vec<Family> {
    let mut analyses = Family::new(
        "postdoc.analyses.finished",
        "",
        Kind::Counter,
        "Analyses finished, by ecosystem and outcome: analyzed, cached, or the error code it ended with.",
    );
    for ((ecosystem, ended), n) in state.flights.outcomes().snapshot() {
        analyses.points.push((
            vec![
                ("ecosystem", ecosystem.to_owned()),
                ("outcome", ended.to_owned()),
            ],
            Point::Int(n),
        ));
    }

    let mut phases = Family::new(
        "postdoc.analysis.phase.duration",
        "s",
        Kind::Histogram,
        "Wall time of each analysis phase, and of each part of classify:report, by phase.",
    );
    for (phase, histogram) in state.phase_durations.snapshot() {
        phases
            .points
            .push((vec![("phase", phase)], Point::Histogram(histogram)));
    }

    let mut families = vec![
        analyses,
        phases,
        Family::gauge(
            "postdoc.analyses.running",
            "",
            "Analyses running now.",
            state.in_flight.len() as u64,
        ),
        Family::gauge(
            "postdoc.analyses.oldest_age",
            "s",
            "Age of the oldest analysis running now; 0 when none is.",
            state
                .in_flight
                .iter()
                .map(|entry| entry.started_at.elapsed().as_secs())
                .max()
                .unwrap_or(0),
        ),
        Family {
            points: vec![(
                Vec::new(),
                Point::Int(state.cap_aborts.load(Ordering::Relaxed)),
            )],
            ..Family::new(
                "postdoc.memory.cap_aborts",
                "",
                Kind::Counter,
                "Analyses aborted to bring RSS back under --max-rss-gb.",
            )
        },
        Family::gauge(
            "postdoc.slots",
            "",
            "Analysis slots this server was sized for.",
            state.config.workers as u64,
        ),
        Family::gauge(
            "postdoc.slots.free",
            "",
            "Analyses this server could start right now.",
            state.available_analysis_permits() as u64,
        ),
        Family::gauge(
            "postdoc.ready",
            "",
            "1 once the model bundle has loaded.",
            u64::from(state.is_ready()),
        ),
        Family::gauge(
            "postdoc.overloaded",
            "",
            "1 while admission is paused for memory.",
            u64::from(state.overload_mark().is_some()),
        ),
        Family::gauge(
            "postdoc.uptime",
            "s",
            "Seconds since this process started.",
            state.started_at.elapsed().as_secs(),
        ),
    ];
    if let Some(rss) = cleave::memory_tracker::current_rss() {
        families.push(Family::gauge(
            "postdoc.memory.rss",
            "By",
            "Resident set size.",
            rss,
        ));
    }

    if let Some(corpus) = &state.corpus {
        let stats = corpus.stats();
        let mut lookups = Family::new(
            "postdoc.corpus.lookups",
            "",
            Kind::Counter,
            "Lookups deferred to the corpus, by result. unreachable reaches the caller as unavailable.",
        );
        for (result, n) in [
            ("found", stats.found),
            ("nothing", stats.nothing),
            ("unreachable", stats.unreachable),
        ] {
            lookups
                .points
                .push((vec![("result", result.to_owned())], Point::Int(n)));
        }
        let mut requests = Family::new(
            "postdoc.corpus.requests",
            "",
            Kind::Counter,
            "Requests to each corpus address, by result.",
        );
        for a in &stats.by_address {
            for (result, n) in [("answered", a.answered), ("failed", a.failed)] {
                requests.points.push((
                    vec![
                        ("address", a.address.clone()),
                        ("result", result.to_owned()),
                    ],
                    Point::Int(n),
                ));
            }
        }
        families.extend([lookups, requests]);
    }

    if let Some(uploader) = &state.uploader {
        let s = uploader.stats();
        let mut renewals = Family::new(
            "postdoc.hopper.renewals",
            "",
            Kind::Counter,
            "Verdict renewals sent to hopper, by result. failed means the work was done and lost.",
        );
        for (result, n) in [("uploaded", s.uploaded), ("failed", s.failed)] {
            renewals
                .points
                .push((vec![("result", result.to_owned())], Point::Int(n as u64)));
        }
        families.push(renewals);
        families.push(Family::gauge(
            "postdoc.hopper.renewals.pending",
            "",
            "Renewals queued for hopper.",
            s.pending as u64,
        ));
    }

    families
}

/// What `obs` puts on every series' resource, so `job` and `instance` mean the
/// same thing for postdoc as for the Go services.
#[derive(Debug)]
struct Resource {
    host: String,
    instance: String,
    environment: String,
}

impl Resource {
    fn detect() -> Self {
        let host = hostname::get()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "unknown".to_owned());
        Self {
            instance: format!("{host}-{}", std::process::id()),
            host,
            environment: std::env::var("OTEL_DEPLOYMENT_ENVIRONMENT")
                .unwrap_or_else(|_| "dev".to_owned()),
        }
    }
}

fn attributes<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Value {
    pairs
        .into_iter()
        .map(|(key, value)| json!({"key": key, "value": {"stringValue": value}}))
        .collect()
}

/// One OTLP `ExportMetricsServiceRequest`, as JSON. Counters are cumulative
/// sums from `start_ns`, which is what the receiver turns into `_total`;
/// histograms are cumulative from it too, in seconds.
fn otlp_json(families: &[Family], resource: &Resource, start_ns: u128, now_ns: u128) -> Value {
    let (start, now) = (start_ns.to_string(), now_ns.to_string());
    let metrics: Vec<Value> = families
        .iter()
        .filter(|family| !family.points.is_empty())
        .map(|family| {
            let points: Vec<Value> = family
                .points
                .iter()
                .map(|(labels, value)| {
                    let mut point = json!({
                        "attributes": attributes(labels.iter().map(|(k, v)| (*k, v.as_str()))),
                        "startTimeUnixNano": start,
                        "timeUnixNano": now,
                    });
                    match value {
                        Point::Int(n) => point["asInt"] = n.to_string().into(),
                        Point::Histogram(h) => {
                            point["count"] = h.count().to_string().into();
                            point["sum"] = (h.sum_ms as f64 / 1_000.0).into();
                            point["bucketCounts"] = h.buckets.iter().map(u64::to_string).collect();
                            point["explicitBounds"] = PHASE_BOUNDS_MS
                                .iter()
                                .map(|&ms| ms as f64 / 1_000.0)
                                .collect();
                        }
                    }
                    point
                })
                .collect();
            let data = match family.kind {
                Kind::Counter => (
                    "sum",
                    json!({
                        "aggregationTemporality": 2,
                        "isMonotonic": true,
                        "dataPoints": points,
                    }),
                ),
                Kind::Gauge => ("gauge", json!({"dataPoints": points})),
                Kind::Histogram => (
                    "histogram",
                    json!({
                        "aggregationTemporality": 2,
                        "dataPoints": points,
                    }),
                ),
            };
            let mut metric = json!({
                "name": family.name,
                "description": family.help,
                "unit": family.unit,
            });
            metric[data.0] = data.1;
            metric
        })
        .collect();
    json!({
        "resourceMetrics": [{
            "resource": {"attributes": attributes([
                ("service.name", "postdoc"),
                ("service.version", env!("CARGO_PKG_VERSION")),
                ("service.instance.id", resource.instance.as_str()),
                ("deployment.environment", resource.environment.as_str()),
                ("host.name", resource.host.as_str()),
            ])},
            "scopeMetrics": [{
                "scope": {"name": "postdoc", "version": env!("CARGO_PKG_VERSION")},
                "metrics": metrics,
            }],
        }],
    })
}

/// Where to push, resolved as `obs` resolves it: the metrics-specific endpoint
/// verbatim, else the base with `/v1/metrics` appended, else the fleet's
/// receiver. `OTEL_METRICS_EXPORTER=none` turns pushing off, as it does for
/// any OTel SDK.
fn push_endpoint(var: impl Fn(&str) -> Option<String>) -> Option<String> {
    let set = |name: &str| var(name).filter(|v| !v.trim().is_empty());
    if set("OTEL_METRICS_EXPORTER").is_some_and(|v| v.trim() == "none") {
        return None;
    }
    if let Some(endpoint) = set("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT") {
        return Some(endpoint);
    }
    if let Some(base) = set("OTEL_EXPORTER_OTLP_ENDPOINT") {
        return Some(format!("{}/v1/metrics", base.trim_end_matches('/')));
    }
    Some(DEFAULT_ENDPOINT.to_owned())
}

/// Push [`collect`] every [`PUSH_EVERY`] until the server stops.
///
/// A box that cannot reach the receiver says so once, at warn, and then only
/// at debug until it can again: the analysis it exists for is unaffected, and
/// a warning every 15s from every worker off the tailnet would bury the logs
/// that matter.
pub(super) fn spawn_pusher(state: &Arc<AppState>) {
    let Some(endpoint) = push_endpoint(|name| std::env::var(name).ok()) else {
        tracing::info!("metrics push disabled (OTEL_METRICS_EXPORTER=none)");
        return;
    };
    let client = match reqwest::Client::builder().timeout(PUSH_TIMEOUT).build() {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(%error, "metrics push disabled: no HTTP client");
            return;
        }
    };
    let resource = Resource::detect();
    // Counters count from process start; OTLP wants that as wall-clock time.
    let start_ns = SystemTime::now()
        .checked_sub(state.started_at.elapsed())
        .unwrap_or_else(SystemTime::now)
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    tracing::info!(%endpoint, instance = %resource.instance, every_secs = PUSH_EVERY.as_secs(), "pushing metrics");
    let weak = Arc::downgrade(state);
    let mut stop = state.shutdown.subscribe();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(PUSH_EVERY);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut failing = false;
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = stop.changed() => return,
            }
            let Some(state) = weak.upgrade() else {
                return;
            };
            let now_ns = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let body = otlp_json(&collect(&state), &resource, start_ns, now_ns);
            drop(state);
            let outcome = match client.post(&endpoint).json(&body).send().await {
                Ok(resp) if resp.status().is_success() => Ok(()),
                Ok(resp) => Err(format!("HTTP {}", resp.status().as_u16())),
                Err(error) => Err(error.to_string()),
            };
            match outcome {
                Ok(()) if failing => {
                    failing = false;
                    tracing::info!(%endpoint, "metrics push recovered");
                }
                Ok(()) => {}
                Err(error) if failing => tracing::debug!(%endpoint, %error, "metrics push failed"),
                Err(error) => {
                    failing = true;
                    tracing::warn!(%endpoint, %error, "metrics push failed; retrying every 15s, logging again on recovery");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::error::ApiError;
    use axum::http::StatusCode;

    fn failed(code: &'static str) -> Outcome {
        Outcome::Failed(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, code, "x"))
    }

    /// rdu2's failure, as the alert will read it: every npm analysis it
    /// finished ended `unretrievable`.
    #[test]
    fn outcomes_count_by_ecosystem_and_code() {
        let outcomes = Outcomes::default();
        let npm = FlightKey::Purl("pkg:npm/left-pad@1.3.0".into());
        outcomes.record(&npm, &failed("unretrievable"));
        outcomes.record(&npm, &failed("unretrievable"));
        outcomes.record(
            &FlightKey::Purl("pkg:cargo/itoa@1.0.11".into()),
            &failed("unretrievable"),
        );
        outcomes.record(&FlightKey::Sha("a".repeat(64)), &failed("internal"));
        outcomes.record(
            &FlightKey::Purl("pkg:gem/rails@7.0.0".into()),
            &failed("invalid_artifact"),
        );
        assert_eq!(
            outcomes.snapshot(),
            vec![
                (("cargo", "unretrievable"), 1),
                (("npm", "unretrievable"), 2),
                (("other", "invalid_artifact"), 1),
                (("sha256", "internal"), 1),
            ]
        );
    }

    #[test]
    fn otlp_counters_are_cumulative_sums_and_gauges_are_gauges() {
        let mut counter = Family::new("postdoc.analyses.finished", "", Kind::Counter, "h");
        counter
            .points
            .push((vec![("outcome", "unretrievable".into())], Point::Int(7)));
        let empty = Family::new("postdoc.corpus.lookups", "", Kind::Counter, "h");
        let gauge = Family::gauge("postdoc.uptime", "s", "h", 42);
        let resource = Resource {
            host: "rdu2".into(),
            instance: "rdu2-1".into(),
            environment: "prod".into(),
        };
        let body = otlp_json(&[counter, empty, gauge], &resource, 1, 2);
        let rm = &body["resourceMetrics"][0];
        assert!(rm["resource"]["attributes"].as_array().is_some_and(|a| {
            a.contains(&json!({"key": "service.name", "value": {"stringValue": "postdoc"}}))
        }));
        let metrics = &rm["scopeMetrics"][0]["metrics"];
        assert_eq!(
            metrics.as_array().map(Vec::len),
            Some(2),
            "an empty family was sent"
        );
        let sum = &metrics[0]["sum"];
        assert_eq!(sum["aggregationTemporality"], 2);
        assert_eq!(sum["isMonotonic"], true);
        assert_eq!(sum["dataPoints"][0]["asInt"], "7");
        assert_eq!(sum["dataPoints"][0]["startTimeUnixNano"], "1");
        assert_eq!(
            sum["dataPoints"][0]["attributes"][0],
            json!({"key": "outcome", "value": {"stringValue": "unretrievable"}})
        );
        assert_eq!(metrics[1]["unit"], "s");
        assert_eq!(metrics[1]["gauge"]["dataPoints"][0]["asInt"], "42");
    }

    /// The parts of `classify:report` are exported beside it, each under its
    /// own `phase`, and with `other` they add up to it.
    #[test]
    fn phase_durations_break_down_classify_report() {
        let phase = scan::analysis::RequestPhase::with_label("req#1 t");
        phase.set("cleave:analyze");
        phase.set("classify:report");
        std::thread::sleep(Duration::from_millis(20));
        phase.add_part("fetch+graft", Duration::from_millis(12));
        phase.add_part("interpret", Duration::from_millis(3));
        let durations = PhaseDurations::default();
        durations.record(&phase.timeline());
        durations.record(&phase.timeline());
        let snapshot = durations.snapshot();
        let sum = |want: &str| {
            snapshot
                .iter()
                .find_map(|(name, h)| (name == want).then_some(h.sum_ms))
                .unwrap_or_default()
        };
        assert!(sum("classify:report") >= 40, "{snapshot:?}");
        assert_eq!(sum("fetch+graft"), 24, "{snapshot:?}");
        assert_eq!(
            sum("fetch+graft") + sum("interpret") + sum("other"),
            sum("classify:report"),
            "{snapshot:?}"
        );
        assert!(snapshot.iter().all(|(_, h)| h.count() == 2), "{snapshot:?}");

        let family = Family {
            points: snapshot
                .into_iter()
                .map(|(name, h)| (vec![("phase", name)], Point::Histogram(h)))
                .collect(),
            ..Family::new("postdoc.analysis.phase.duration", "s", Kind::Histogram, "h")
        };
        let resource = Resource {
            host: "h".into(),
            instance: "h-1".into(),
            environment: "test".into(),
        };
        let body = otlp_json(&[family], &resource, 1, 2);
        let metric = &body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0];
        assert_eq!(metric["name"], "postdoc.analysis.phase.duration");
        assert_eq!(metric["unit"], "s");
        let histogram = &metric["histogram"];
        assert_eq!(histogram["aggregationTemporality"], 2);
        let fetch = histogram["dataPoints"]
            .as_array()
            .and_then(|points| {
                points.iter().find(|p| {
                    p["attributes"][0]
                        == json!({"key": "phase", "value": {"stringValue": "fetch+graft"}})
                })
            })
            .cloned()
            .unwrap_or_default();
        assert_eq!(fetch["count"], "2");
        assert_eq!(fetch["sum"], 0.024);
        // 12ms lands in the (10ms, 50ms] bucket.
        assert_eq!(fetch["bucketCounts"][1], "2");
        assert_eq!(
            fetch["bucketCounts"].as_array().map(Vec::len),
            Some(PHASE_BOUNDS_MS.len() + 1)
        );
        assert_eq!(fetch["explicitBounds"][0], 0.01);
    }

    #[test]
    fn push_endpoint_resolves_like_obs() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        assert_eq!(push_endpoint(env(&[])).as_deref(), Some(DEFAULT_ENDPOINT));
        assert_eq!(
            push_endpoint(env(&[(
                "OTEL_EXPORTER_OTLP_ENDPOINT",
                "http://o:9090/api/v1/otlp/"
            )]))
            .as_deref(),
            Some("http://o:9090/api/v1/otlp/v1/metrics")
        );
        assert_eq!(
            push_endpoint(env(&[
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://base"),
                ("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", "http://exact/path"),
            ]))
            .as_deref(),
            Some("http://exact/path")
        );
        assert_eq!(
            push_endpoint(env(&[("OTEL_METRICS_EXPORTER", "none")])),
            None
        );
    }
}
