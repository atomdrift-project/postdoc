//! Shared helpers for the `/v1` routes and `GET /status` (kept for
//! beamline's reconnect routing). The legacy `/analyze`, `/analyze-purl`,
//! `/analyze-path`, `/lookup` and admin `/_/reload`, `/_/update` routes
//! were removed: beamline uses only `/v1/*`, and promoter now uploads
//! bytes to `POST /v1/analyze?full=1`.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use std::sync::Arc;

use super::AppState;
use super::analyze::index_query;
use super::error::ApiError;
use super::flight::FlightKey;

/// The name an upload is staged and reported under.
///
/// Every character outside `[A-Za-z0-9_.-]` becomes `_`, `..` collapses to
/// `__`, and the result keeps its last 63 bytes so the extension survives —
/// cleave detects file type from it, and the name is also the `path` label in
/// the report and in every log line about this request.
///
/// The filter is deliberately ASCII-only rather than Unicode-aware. Two
/// reasons, both of them the client's choice to make otherwise: a
/// `char::is_alphanumeric` filter keeps multi-byte characters, and the
/// right-truncation below would then slice mid-character and panic the
/// request; and a name that reaches logs and a filesystem path should not
/// carry homoglyphs, combining marks, or bidi-shaped text.
pub(super) fn sanitize_upload_filename(raw: &str) -> String {
    let sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .replace("..", "__");
    // Every retained character is one ASCII byte, so this index is always a
    // character boundary.
    #[expect(
        clippy::string_slice,
        reason = "every retained character is one ASCII byte"
    )]
    if sanitized.len() > 63 {
        sanitized[sanitized.len() - 63..].to_string()
    } else {
        sanitized
    }
}

/// Canonical `pkg:…` form, or a 400 message. Same prefixing rule as `atomscan purl`.
pub(super) fn normalize_pkg_purl(raw: &str) -> Result<String, &'static str> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("missing purl");
    }
    let prefixed = if raw.starts_with("pkg:") {
        raw.to_string()
    } else {
        format!("pkg:{raw}")
    };
    fletch::purl::normalize(&prefixed).ok_or("not a package URL")
}

/// Whether `raw` is an absolute http(s) URL. Callers keep and log `raw`
/// itself, and `Url::parse` silently drops tab, CR and LF, so a raw control
/// character (a forged log line, a terminal escape) is refused outright; a
/// real URL percent-encodes them.
pub(super) fn valid_http_url(raw: &str) -> bool {
    !raw.chars().any(char::is_control)
        && reqwest::Url::parse(raw).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
}

/// Render the shared outcome as this request's response. `elapsed_ms` is the
/// caller's own wall time, so a follower reports how long *it* waited, `shared`
/// marks the response as one that rode another request's analysis, and `key`
/// names the artifact on the access line.
pub(super) fn pick_verdict(
    by_sha: Option<scan::lookup::Verdict>,
    by_purl: impl FnOnce() -> Option<scan::lookup::Verdict>,
    sha: &str,
) -> Option<scan::lookup::Verdict> {
    if by_sha.is_some() {
        return by_sha;
    }
    by_purl().filter(|v| v.sha256.eq_ignore_ascii_case(sha))
}

///
/// Both identifiers travel as query parameters. A PURL's own grammar carries
/// `/`, `?` and `#` — `pkg:npm/@scope/name@1.0.0?arch=x64` in a path segment
/// would have everything from the `?` parsed as the *URL's* query and a
/// `#subpath` dropped by the client, silently keying on a different package —
/// and a digest gains nothing from a prettier URL that the other key cannot
/// have too.
#[derive(Debug, Default, serde::Deserialize)]
pub(super) struct LookupQuery {
    sha256: Option<String>,
    purl: Option<String>,
    url: Option<String>,
}

/// A 400 naming what the caller sent wrong.
fn lookup_error(status: StatusCode, message: &'static str) -> Response {
    ApiError::new(status, "bad_request", message).into_response()
}

#[derive(serde::Serialize)]
pub(super) struct StatusBody {
    pub(super) state: &'static str,
    pub(super) purl: Option<String>,
    pub(super) url: Option<String>,
    pub(super) sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) elapsed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) attached: Option<usize>,
}

/// GET /status?sha256=… | ?purl=… | ?url=… — where an analysis of this artifact stands.
///
/// Exists for the caller whose connection did not survive the analysis. The run
/// keeps going here when a proxy gives up at its own ceiling, but from outside
/// a run in progress and a run that never started are both `404 unknown sample`
/// on /lookup. That ambiguity is the whole problem: it is the difference
/// between waiting a little longer and paying for a twenty-minute analysis
/// twice.
///
/// Running is reported before complete, so a caller is never told to go away
/// while a run it could ride is still live. A run's verdict is indexed before
/// its flight is retired, so there is no window where it reads as neither.
///
/// `lost` is deliberately not a state. It is the caller's own inference: they
/// dispatched, their connection died, and this answers `unknown`. Reporting it
/// here would mean keeping a graveyard of every run that ever ended, to tell a
/// caller something they already know.
pub(super) async fn status(
    State(state): State<Arc<AppState>>,
    Query(q): Query<LookupQuery>,
) -> Response {
    let sha = q.sha256.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let raw = q.purl.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let raw_url = q.url.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if raw.is_some() && raw_url.is_some() {
        return lookup_error(StatusCode::BAD_REQUEST, "provide purl or url, not both");
    }
    if let Some(url) = raw_url
        && !valid_http_url(url)
    {
        return lookup_error(StatusCode::BAD_REQUEST, "invalid url");
    }
    if sha.is_none() && raw.is_none() && raw_url.is_none() {
        return lookup_error(
            StatusCode::BAD_REQUEST,
            "provide sha256, purl, url, or both",
        );
    }
    // The canonical form is what a flight is keyed by, so an uncanonical spelling
    // must not read as a different artifact — the same rule /lookup follows.
    let purl = match raw.map(normalize_pkg_purl) {
        Some(Ok(purl)) => Some(purl),
        Some(Err(message)) => return lookup_error(StatusCode::BAD_REQUEST, message),
        None => None,
    };
    let sha = sha.map(str::to_ascii_lowercase);
    let url = raw_url.map(str::to_owned);

    let running = purl
        .as_deref()
        .and_then(|p| state.flights.running(&FlightKey::Purl(p.to_string())))
        .or_else(|| {
            url.as_deref()
                .and_then(|u| state.flights.running(&FlightKey::Url(u.to_string())))
        })
        .or_else(|| {
            sha.as_deref()
                .and_then(|s| state.flights.running(&FlightKey::Sha(s.to_string())))
        });
    if let Some(run) = running {
        return Json(StatusBody {
            state: "running",
            purl,
            url,
            sha256: sha,
            elapsed_ms: Some(scan::duration_ms(run.elapsed)),
            attached: Some(run.attached),
        })
        .into_response();
    }

    let (key_purl, key_sha) = (purl.clone(), sha.clone());
    let complete = index_query(move |index| {
        key_purl
            .as_deref()
            .is_some_and(|p| index.get_purl(p).is_some())
            || key_sha
                .as_deref()
                .is_some_and(|s| index.get_sha(s).is_some())
    })
    .await
    .unwrap_or(false);
    Json(StatusBody {
        state: if complete { "complete" } else { "unknown" },
        purl,
        url,
        sha256: sha,
        elapsed_ms: None,
        attached: None,
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Url::parse` drops tab/CR/LF, but the raw string is what gets stored and
    /// logged: a raw control character must be refused, not normalized away.
    #[test]
    fn submitted_urls_refuse_raw_control_characters() {
        assert!(valid_http_url("https://a.example/x?q=%0A"));
        for raw in [
            "https://a.example/x\nINFO forged",
            "https://a.example/x\r\n",
            "https://a.example/\tx",
            "https://a.example/\u{1b}[2J",
            "file:///etc/passwd",
        ] {
            assert!(!valid_http_url(raw), "{raw:?}");
        }
    }

    /// A hostile upload filename must not be able to panic the request. A
    /// Unicode-aware filter kept multi-byte characters, and the tail
    /// truncation then sliced one in half.
    #[test]
    fn sanitize_upload_filename_survives_multibyte_names() {
        let raw = "\u{3041}".repeat(80) + ".zip";
        let name = sanitize_upload_filename(&raw);
        assert_eq!(name.len(), 63);
        assert!(name.is_ascii(), "{name}");
        assert!(name.ends_with(".zip"), "the extension must survive: {name}");
    }

    #[test]
    fn sanitize_upload_filename_defuses_paths_and_control_characters() {
        assert_eq!(
            sanitize_upload_filename("../../etc/shadow"),
            "______etc_shadow"
        );
        assert_eq!(
            sanitize_upload_filename("a\nb\r\u{202e}gpj.exe"),
            "a_b__gpj.exe"
        );
        // A name already inside the alphabet is left exactly as it is.
        assert_eq!(
            sanitize_upload_filename("left-pad-1.3.0.tgz"),
            "left-pad-1.3.0.tgz"
        );
    }

    #[test]
    fn normalize_pkg_purl_accepts_bare_and_full() {
        assert_eq!(
            normalize_pkg_purl("pkg:npm/left-pad@1.3.0").unwrap(),
            "pkg:npm/left-pad@1.3.0"
        );
        assert_eq!(
            normalize_pkg_purl("npm/left-pad@1.3.0").unwrap(),
            "pkg:npm/left-pad@1.3.0"
        );
        assert!(normalize_pkg_purl("").is_err());
        assert!(normalize_pkg_purl("not a purl").is_err());
    }
}

#[cfg(test)]
mod pick_verdict_tests {
    use super::pick_verdict;
    use scan::lookup::Verdict;

    fn verdict(sha: &str, eng: &str) -> Verdict {
        Verdict {
            sha256: sha.to_owned(),
            lvl: scan::model::Level::Clean,
            eng: eng.to_owned(),
            at: "2026-01-01T00:00:00Z".to_owned(),
            purl: None,
            why: None,
            hits: Vec::new(),
        }
    }

    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn a_digest_hit_wins_and_costs_no_purl_lookup() {
        let mut asked = false;
        let got = pick_verdict(
            Some(verdict(SHA, "by-sha")),
            || {
                asked = true;
                Some(verdict(SHA, "by-purl"))
            },
            SHA,
        );
        assert_eq!(got.expect("verdict").eng, "by-sha");
        assert!(!asked, "an exact hit must not cost a second index lookup");
    }

    // The second chance: the index knows the release even though these exact
    // bytes are new to it, and the digests agree.
    #[test]
    fn the_purl_answers_when_the_digest_is_unknown() {
        let got = pick_verdict(None, || Some(verdict(SHA, "by-purl")), SHA);
        assert_eq!(got.expect("verdict").eng, "by-purl");
    }

    // The guard the pair exists for: the release resolved to other bytes.
    #[test]
    fn a_purl_verdict_for_other_bytes_is_refused() {
        let got = pick_verdict(None, || Some(verdict(OTHER, "different-artifact")), SHA);
        assert!(
            got.is_none(),
            "served a verdict about bytes nobody asked about"
        );
    }

    #[test]
    fn digest_comparison_is_case_insensitive() {
        let got = pick_verdict(None, || Some(verdict(&SHA.to_uppercase(), "by-purl")), SHA);
        assert!(got.is_some(), "hex case must not decide identity");
    }

    #[test]
    fn neither_key_known_is_no_verdict() {
        assert!(pick_verdict(None, || None, SHA).is_none());
    }
}
