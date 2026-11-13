//! Chainguard's malcontent, run once per artifact.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Input, failed, file_name, find_program, run_tool, tool_version};
use crate::bounded;

/// Longest one malcontent run may take before it is killed.
const MALCONTENT_TIMEOUT: Duration = Duration::from_secs(600);

/// The most JSON malcontent may print. Only high and critical findings are
/// asked for, so a real report is far smaller.
const MAX_MALCONTENT_OUTPUT: u64 = 64 << 20;

/// How many behaviour ids an opinion names, at most: the worst ones, enough
/// to say what was found while staying inside hopper's object limit.
const MAX_BEHAVIOURS: usize = 5;

#[derive(Debug)]
pub(super) struct Malcontent {
    bin: PathBuf,
    /// Its version; its rules ship inside it, so this names them too.
    engine: String,
}

impl Malcontent {
    /// malcontent at `bin`, or `mal` where it can be found, asked its
    /// version to prove it runs.
    pub(super) fn start(bin: Option<PathBuf>) -> Result<Self> {
        let bin = bin.or_else(|| find_program("mal")).ok_or_else(|| {
            anyhow::anyhow!("malcontent: no `mal` on PATH (is malcontent installed?)")
        })?;
        let engine = tool_version(&bin).map_err(|e| anyhow::anyhow!("malcontent: {e}"))?;
        tracing::info!(bin = %bin.display(), engine, "malcontent ready");
        Ok(Self { bin, engine })
    }

    pub(super) fn bin(&self) -> &Path {
        &self.bin
    }

    pub(super) fn scan(&self, input: Input<'_>) -> Value {
        let mut object = self.scan_input(input).unwrap_or_else(|failure| failure);
        if let Some(fields) = object.as_object_mut() {
            // Its rules ship inside the binary, so its version is also what
            // it judged by.
            fields.insert("engine".to_owned(), self.engine.clone().into());
            fields.insert("ruleset".to_owned(), self.engine.clone().into());
        }
        object
    }

    /// The opinion, or the failed object to record instead.
    fn scan_input(&self, input: Input<'_>) -> Result<Value, Value> {
        match input {
            Input::Path(path) => self.run(path),
            Input::Bytes { bytes, name } => {
                // malcontent reads paths, and types a file by its extension,
                // so the bytes go to disk under the artifact's own name.
                let dir = tempfile::Builder::new()
                    .prefix("postdoc-malcontent")
                    .tempdir()
                    .map_err(|e| failed("error", &format!("staging for malcontent: {e}")))?;
                let path = dir.path().join(file_name(name));
                std::fs::write(&path, bytes)
                    .map_err(|e| failed("error", &format!("staging for malcontent: {e}")))?;
                self.run(&path)
            }
        }
    }

    fn run(&self, path: &Path) -> Result<Value, Value> {
        let mut cmd = Command::new(&self.bin);
        // One job: postdoc already runs one analysis per slot, so
        // malcontent's own fan-out would only oversubscribe the host.
        cmd.args([
            "--format",
            "json",
            "--min-risk",
            "high",
            "--min-file-risk",
            "high",
            "--jobs",
            "1",
            "analyze",
        ])
        .arg(path);
        let ran = run_tool(cmd, "malcontent", MALCONTENT_TIMEOUT, MAX_MALCONTENT_OUTPUT)?;
        if !ran.status.success() {
            let tail = String::from_utf8_lossy(&ran.stderr);
            return Err(failed(
                "error",
                &format!("malcontent {}: {tail}", ran.status),
            ));
        }
        malcontent_opinion(&ran.stdout)
    }
}

#[derive(Deserialize)]
struct MalReport {
    #[serde(rename = "Files", default)]
    files: std::collections::BTreeMap<String, MalFile>,
}

#[derive(Deserialize)]
struct MalFile {
    #[serde(rename = "RiskScore", default)]
    risk: u8,
    #[serde(rename = "Behaviors", default)]
    behaviors: Vec<MalBehavior>,
}

#[derive(Deserialize)]
struct MalBehavior {
    #[serde(rename = "ID", default)]
    id: String,
    #[serde(rename = "RiskScore", default)]
    risk: u8,
}

/// malcontent's report as an opinion: its worst file's risk on the shared
/// scale (critical is hostile, high suspicious, anything less benign), how
/// many files reached it, and the behaviours that put them there.
fn malcontent_opinion(stdout: &[u8]) -> Result<Value, Value> {
    let report: MalReport = serde_json::from_slice(stdout)
        .map_err(|e| failed("error", &format!("malcontent report: {e}")))?;
    let worst = report.files.values().map(|f| f.risk).max().unwrap_or(0);
    let (class, risk) = match worst {
        4.. => ("hostile", "critical"),
        3 => ("suspicious", "high"),
        _ => return Ok(json!({"class": "benign"})),
    };
    let at_worst = || report.files.values().filter(|f| f.risk == worst);
    let found: Vec<&MalBehavior> = at_worst()
        .flat_map(|f| f.behaviors.iter())
        .filter(|b| !b.id.is_empty())
        .collect();
    // Several lesser behaviours together can raise a file a level, so the
    // strongest present are named, whether or not one reaches it alone.
    let strongest = found.iter().map(|b| b.risk).max().unwrap_or(0);
    let mut behaviors: Vec<&str> = found
        .iter()
        .filter(|b| b.risk == strongest)
        .map(|b| b.id.as_str())
        .collect();
    behaviors.sort_unstable();
    behaviors.dedup();
    behaviors.truncate(MAX_BEHAVIOURS);
    Ok(json!({
        "class": class,
        "risk": risk,
        "files": at_worst().count(),
        "behaviors": behaviors.iter().map(|id| bounded(id, 120)).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malcontents_worst_file_is_the_artifacts_reading() {
        let report = r#"{"Files":{
            "a.tgz ∴ /package/index.js":{"RiskScore":3,"RiskLevel":"HIGH","Behaviors":[
                {"ID":"exec/remote_commands/code_eval","RiskScore":3},
                {"ID":"net/download","RiskScore":2}]},
            "a.tgz ∴ /package/install.js":{"RiskScore":4,"RiskLevel":"CRITICAL","Behaviors":[
                {"ID":"malware/family/x","RiskScore":4},
                {"ID":"exec/shell","RiskScore":3}]}}}"#
            .as_bytes();
        assert_eq!(
            malcontent_opinion(report),
            Ok(json!({
                "class": "hostile",
                "risk": "critical",
                "files": 1,
                "behaviors": ["malware/family/x"],
            }))
        );
        // Critical through several high behaviours, none critical alone
        // (as malcontent rates a minified @solana/web3.js bundle).
        let combined = r#"{"Files":{"a.tgz ∴ /package/lib/index.iife.min.js":{"RiskScore":4,
            "Behaviors":[{"ID":"credential/ssl/key","RiskScore":3},
                {"ID":"anti-static/obfuscation/js","RiskScore":3},
                {"ID":"net/url","RiskScore":2}]}}}"#
            .as_bytes();
        assert_eq!(
            malcontent_opinion(combined),
            Ok(json!({
                "class": "hostile",
                "risk": "critical",
                "files": 1,
                "behaviors": ["anti-static/obfuscation/js", "credential/ssl/key"],
            }))
        );
        assert_eq!(
            malcontent_opinion(br#"{"Files":{}}"#),
            Ok(json!({"class": "benign"}))
        );
        assert_eq!(malcontent_opinion(b"{}"), Ok(json!({"class": "benign"})));
        assert_eq!(
            malcontent_opinion(b"not json").map_err(|e| e["outcome"].clone()),
            Err(json!("error"))
        );
    }
}
