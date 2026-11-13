//! Datadog's GuardDog, on the packages it knows.
//!
//! GuardDog reads a package's unpacked tree, not its archive, so the artifact
//! is unpacked into a temporary directory first, with the entry count and the
//! bytes written both capped against an archive bomb and every entry kept
//! inside the directory. Without `--metadata` only its source rules run, so a
//! scan stays offline.
//!
//! Graded on GuardDog's own risk score (0 to 10), which folds its threat
//! rules, capabilities and metadata signals into one calibrated number; its
//! rules fire one by one on benign packages too. The bands are GuardDog's:
//! 7 and up is likely malicious, under 5 likely benign, between them a
//! question for a person.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{Input, failed, find_program, run_tool, tool_version};
use crate::bounded;

/// Longest one GuardDog run may take before it is killed.
const GUARDDOG_TIMEOUT: Duration = Duration::from_secs(600);

/// The most output GuardDog may print. Its report is one JSON object.
const MAX_GUARDDOG_OUTPUT: u64 = 16 << 20;

/// The risk at and above which GuardDog calls a package malicious.
const HOSTILE_RISK: f64 = 7.0;

/// The risk at and above which a package is worth a person's look.
const SUSPICIOUS_RISK: f64 = 5.0;

/// How many threat rules an opinion names, at most.
const MAX_THREATS: usize = 5;

/// The most entries an unpacked package may hold.
const MAX_FILES: usize = 20_000;

/// The most bytes an unpacked package may expand to.
const MAX_BYTES: u64 = 512 << 20;

/// GuardDog confines itself with a kernel sandbox (Landlock on Linux,
/// `sandbox_init` on macOS) and refuses to scan without one unless told to.
/// Elsewhere there is no sandbox to ask for, and the service's own
/// confinement is all there is.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const SANDBOX: &[&str] = &[];
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const SANDBOX: &[&str] = &["--no-sandbox"];

#[derive(Debug)]
pub(super) struct GuardDog {
    bin: PathBuf,
    /// Its version, when it says one; its rules ship inside it.
    engine: Option<String>,
}

impl GuardDog {
    /// GuardDog at `bin`, or `guarddog` where it can be found, proved by a
    /// scan of an empty npm package: the sandbox it insists on only fails
    /// when it is asked to scan something.
    pub(super) fn start(bin: Option<PathBuf>) -> Result<Self> {
        let bin = bin.or_else(|| find_program("guarddog")).ok_or_else(|| {
            anyhow::anyhow!("guarddog: no `guarddog` on PATH (is GuardDog installed?)")
        })?;
        let engine = tool_version(&bin).ok();
        let dog = Self { bin, engine };
        let probe = tempfile::Builder::new()
            .prefix("postdoc-guarddog-probe")
            .tempdir()?;
        std::fs::write(
            probe.path().join("package.json"),
            r#"{"name":"postdoc-probe","version":"0.0.0"}"#,
        )?;
        if let Err(failure) = dog.run("npm", probe.path()) {
            anyhow::bail!(
                "guarddog: {} could not scan an empty npm package: {}",
                dog.bin.display(),
                failure["error"].as_str().unwrap_or_default()
            );
        }
        tracing::info!(bin = %dog.bin.display(), engine = dog.engine.as_deref().unwrap_or("unknown"), "guarddog ready");
        Ok(dog)
    }

    pub(super) fn bin(&self) -> &Path {
        &self.bin
    }

    /// What GuardDog makes of `input`, a package of type `file_type` (scan's
    /// name for it). `None` when it is not a package GuardDog reads.
    pub(super) fn scan(&self, input: Input<'_>, file_type: &str) -> Option<Value> {
        let ecosystem = ecosystem(file_type)?;
        let mut object = self
            .scan_package(ecosystem, input)
            .unwrap_or_else(|failure| failure);
        if let (Some(engine), Some(fields)) = (&self.engine, object.as_object_mut()) {
            fields.insert("engine".to_owned(), engine.clone().into());
            fields.insert("ruleset".to_owned(), engine.clone().into());
        }
        Some(object)
    }

    fn scan_package(&self, ecosystem: &str, input: Input<'_>) -> Result<Value, Value> {
        let dir = tempfile::Builder::new()
            .prefix("postdoc-guarddog")
            .tempdir()
            .map_err(|e| failed("error", &format!("unpacking for guarddog: {e}")))?;
        unpack(input, dir.path())
            .map_err(|e| failed("unsupported", &format!("unpacking for guarddog: {e}")))?;
        self.run(ecosystem, dir.path())
    }

    fn run(&self, ecosystem: &str, dir: &Path) -> Result<Value, Value> {
        let mut cmd = Command::new(&self.bin);
        cmd.args([ecosystem, "scan"])
            .arg(dir)
            .args(["--output-format", "json"])
            .args(SANDBOX);
        let ran = run_tool(cmd, "guarddog", GUARDDOG_TIMEOUT, MAX_GUARDDOG_OUTPUT)?;
        guarddog_opinion(&ran.stdout).map_err(|failure| {
            if ran.status.success() {
                return failure;
            }
            let tail = String::from_utf8_lossy(&ran.stderr);
            failed("error", &format!("guarddog {}: {tail}", ran.status))
        })
    }
}

/// The GuardDog ecosystem for one of scan's package types.
fn ecosystem(file_type: &str) -> Option<&'static str> {
    match file_type {
        "npm" => Some("npm"),
        "whl" | "python_sdist" | "egg" => Some("pypi"),
        "go_module" => Some("go"),
        "gem" => Some("rubygems"),
        _ => None,
    }
}

#[derive(Deserialize)]
struct Report {
    #[serde(default)]
    errors: Map<String, Value>,
    #[serde(default)]
    results: Map<String, Value>,
    risk_score: Option<Risk>,
}

#[derive(Deserialize)]
struct Risk {
    #[serde(default)]
    label: String,
    score: f64,
}

/// GuardDog's report as an opinion: the band its risk score falls in, the
/// score and GuardDog's label for it, and the threat rules that fired.
fn guarddog_opinion(stdout: &[u8]) -> Result<Value, Value> {
    // A banner printed before the report is skipped.
    let report: Report = serde_json::from_slice(stdout)
        .or_else(|first| {
            let start = stdout.iter().position(|&b| b == b'{').ok_or(first)?;
            serde_json::from_slice(stdout.get(start..).unwrap_or_default())
        })
        .map_err(|e| failed("error", &format!("guarddog report: {e}")))?;
    let risk = report.risk_score.ok_or_else(|| {
        failed(
            "error",
            "guarddog's report has no risk score (GuardDog 3 or later is needed)",
        )
    })?;
    let class = if risk.score >= HOSTILE_RISK {
        "hostile"
    } else if risk.score >= SUSPICIOUS_RISK {
        "suspicious"
    } else if report.errors.is_empty() {
        "benign"
    } else {
        // A clean score from rules that did not all run is not a reading.
        return Err(failed(
            "error",
            &format!("guarddog: {} of its rules failed", report.errors.len()),
        ));
    };
    let mut threats: Vec<&str> = report
        .results
        .iter()
        .filter(|(_, found)| fired(found))
        .filter_map(|(rule, _)| rule.strip_prefix("threat-"))
        .collect();
    threats.sort_unstable();
    threats.truncate(MAX_THREATS);
    Ok(json!({
        "class": class,
        "risk": risk.score,
        "label": bounded(&risk.label, 60),
        "threats": threats.iter().map(|rule| bounded(rule, 120)).collect::<Vec<_>>(),
    }))
}

/// Whether a rule's result says it found something.
fn fired(found: &Value) -> bool {
    match found {
        Value::Null | Value::Bool(false) => false,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        Value::Bool(true) | Value::Number(_) => true,
    }
}

/// Unpack `input` into `dir`: a gzipped tar (npm, sdists), a zip (wheels, Go
/// modules), a plain tar (gems), or else the file itself as `source`.
fn unpack(input: Input<'_>, dir: &Path) -> io::Result<()> {
    match input {
        Input::Path(path) => unpack_from(File::open(path)?, dir),
        Input::Bytes { bytes, .. } => unpack_from(io::Cursor::new(bytes), dir),
    }
}

fn unpack_from<R: Read + Seek>(mut reader: R, dir: &Path) -> io::Result<()> {
    let mut magic = [0u8; 512];
    let mut seen = 0;
    while seen < magic.len() {
        let n = reader.read(magic.get_mut(seen..).unwrap_or_default())?;
        if n == 0 {
            break;
        }
        seen += n;
    }
    reader.seek(SeekFrom::Start(0))?;
    let mut budget = Budget::default();
    match magic {
        [0x1f, 0x8b, ..] => untar(flate2::read::GzDecoder::new(reader), dir, &mut budget),
        [b'P', b'K', ..] => unzip(reader, dir, &mut budget),
        _ if magic.get(257..262) == Some(b"ustar".as_slice()) => untar(reader, dir, &mut budget),
        _ => budget.write(&mut reader, &dir.join("source")),
    }
}

fn untar(reader: impl Read, dir: &Path, budget: &mut Budget) -> io::Result<()> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        // Links and devices are skipped: a link could point outside `dir`,
        // and GuardDog reads files.
        if !(kind.is_file() || kind.is_dir()) {
            continue;
        }
        let Some(dst) = inside(dir, &entry.path()?) else {
            continue;
        };
        budget.entry()?;
        if kind.is_dir() {
            std::fs::create_dir_all(&dst)?;
        } else {
            budget.write(&mut entry, &dst)?;
        }
    }
    Ok(())
}

fn unzip(reader: impl Read + Seek, dir: &Path, budget: &mut Budget) -> io::Result<()> {
    let mut archive = zip::ZipArchive::new(reader).map_err(io::Error::other)?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(io::Error::other)?;
        let Some(dst) = file.enclosed_name().and_then(|name| inside(dir, &name)) else {
            continue;
        };
        budget.entry()?;
        if file.is_dir() {
            std::fs::create_dir_all(&dst)?;
        } else if file.is_file() {
            budget.write(&mut file, &dst)?;
        }
    }
    Ok(())
}

/// `name` under `dir`, or `None` when it would leave it.
fn inside(dir: &Path, name: &Path) -> Option<PathBuf> {
    let mut out = dir.to_path_buf();
    for part in name.components() {
        match part {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (out != dir).then_some(out)
}

/// What unpacking one package may still spend.
#[derive(Default)]
struct Budget {
    entries: usize,
    bytes: u64,
}

impl Budget {
    fn entry(&mut self) -> io::Result<()> {
        self.entries += 1;
        if self.entries > MAX_FILES {
            return Err(io::Error::other(format!("more than {MAX_FILES} entries")));
        }
        Ok(())
    }

    fn write(&mut self, src: &mut impl Read, dst: &Path) -> io::Result<()> {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let left = MAX_BYTES.saturating_sub(self.bytes);
        let written = io::copy(&mut src.take(left + 1), &mut File::create(dst)?)?;
        if written > left {
            return Err(io::Error::other(format!(
                "expands past {} MiB",
                MAX_BYTES >> 20
            )));
        }
        self.bytes += written;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    #[test]
    fn guarddog_reads_only_the_packages_it_knows() {
        assert_eq!(ecosystem("npm"), Some("npm"));
        assert_eq!(ecosystem("whl"), Some("pypi"));
        assert_eq!(ecosystem("python_sdist"), Some("pypi"));
        assert_eq!(ecosystem("go_module"), Some("go"));
        assert_eq!(ecosystem("gem"), Some("rubygems"));
        assert_eq!(ecosystem("crate"), None);
        assert_eq!(ecosystem("elf"), None);
    }

    #[test]
    fn the_risk_score_decides_the_band() {
        let report = |score: f64, errors: &str| {
            format!(
                r#"{{"issues":2,"errors":{errors},"results":{{"threat-shady-links":{{"a.js":"x"}},"threat-exec-base64":[],"npm-install-script":{{"package.json":"y"}}}},"risk_score":{{"label":"malicious","score":{score}}}}}"#
            )
        };
        assert_eq!(
            guarddog_opinion(report(8.5, "{}").as_bytes()),
            Ok(
                json!({"class": "hostile", "risk": 8.5, "label": "malicious", "threats": ["shady-links"]})
            )
        );
        assert_eq!(
            guarddog_opinion(report(5.0, "{}").as_bytes()).map(|o| o["class"].clone()),
            Ok(json!("suspicious"))
        );
        assert_eq!(
            guarddog_opinion(report(1.0, "{}").as_bytes()).map(|o| o["class"].clone()),
            Ok(json!("benign"))
        );
        assert_eq!(
            guarddog_opinion(report(1.0, r#"{"rule":"boom"}"#).as_bytes())
                .map_err(|o| o["outcome"].clone()),
            Err(json!("error")),
            "a clean score from rules that failed is no reading"
        );
    }

    #[test]
    fn a_banner_is_skipped_and_an_old_guarddog_is_named() {
        let with_banner = br#"Scanning... {"errors":{},"results":{},"risk_score":{"label":"no_risks_detected","score":0.0}}"#;
        assert_eq!(
            guarddog_opinion(with_banner).map(|o| o["class"].clone()),
            Ok(json!("benign"))
        );
        let old = guarddog_opinion(br#"{"issues":0,"errors":{},"results":{}}"#)
            .err()
            .unwrap_or_default();
        assert!(
            old["error"]
                .as_str()
                .unwrap_or_default()
                .contains("GuardDog 3")
        );
    }

    fn tgz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        {
            let mut builder = tar::Builder::new(&mut out);
            for (name, data) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, name, *data)
                    .expect("entry");
            }
            let mut link = tar::Header::new_gnu();
            link.set_entry_type(tar::EntryType::Symlink);
            link.set_size(0);
            builder
                .append_link(&mut link, "package/escape", "/etc/passwd")
                .expect("link");
            builder.finish().expect("finish");
        }
        out.finish().expect("gzip")
    }

    #[test]
    fn a_package_unpacks_without_its_links() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bytes = tgz(&[("package/package.json", b"{}"), ("package/index.js", b"x")]);
        unpack(
            Input::Bytes {
                bytes: &bytes,
                name: "a.tgz",
            },
            dir.path(),
        )
        .expect("unpacks");
        assert!(dir.path().join("package/index.js").is_file());
        assert!(
            !dir.path().join("package/escape").exists(),
            "links are skipped"
        );
    }

    #[test]
    fn a_wheel_unpacks_and_a_bare_file_is_kept_as_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut wheel = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        wheel
            .start_file("pkg/__init__.py", zip::write::SimpleFileOptions::default())
            .expect("entry");
        wheel.write_all(b"print(1)").expect("write");
        let wheel = wheel.finish().expect("zip").into_inner();
        unpack(
            Input::Bytes {
                bytes: &wheel,
                name: "a.whl",
            },
            dir.path(),
        )
        .expect("unpacks");
        assert!(dir.path().join("pkg/__init__.py").is_file());

        let bare = tempfile::tempdir().expect("tempdir");
        unpack(
            Input::Bytes {
                bytes: b"console.log(1)",
                name: "a.js",
            },
            bare.path(),
        )
        .expect("copies");
        assert_eq!(
            std::fs::read(bare.path().join("source")).ok(),
            Some(b"console.log(1)".to_vec())
        );
    }

    #[test]
    fn an_entry_cannot_leave_its_directory() {
        let dir = Path::new("/tmp/x");
        assert_eq!(
            inside(dir, Path::new("a/b")),
            Some(PathBuf::from("/tmp/x/a/b"))
        );
        assert_eq!(inside(dir, Path::new("../etc/passwd")), None);
        assert_eq!(inside(dir, Path::new("/etc/passwd")), None);
        assert_eq!(inside(dir, Path::new(".")), None);
    }
}
