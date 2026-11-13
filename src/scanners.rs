//! Third-party scanners: what ClamAV, malcontent and GuardDog say of an
//! artifact.
//!
//! Each opinion goes into scan's upload under `scanners`, keyed by the
//! scanner's name (scan's `JSON_PLAN.md`, "Other scanners"). A scanner never
//! feeds atomdrift's verdict; hopper stores what each said and decides what
//! they add up to when a lookup is answered.
//!
//! postdoc runs all three by default. Each is found (or named on the command
//! line) and checked when postdoc starts, and a postdoc that cannot run one it
//! was asked for refuses to start, saying which and why, rather than posting
//! results that quietly lack it. `--scanners` narrows the set, or turns them
//! off with `none`.
//!
//! A scanner that ran always reports: a failure is `{"outcome":"error"}` (or
//! `timeout`, or `unsupported`), which leaves its last good finding standing.
//! One that did not run is left out, which hopper reads as no statement.
//! GuardDog reads only the packages it knows (npm, PyPI, Go, RubyGems); for
//! anything else it does not run.

mod clamav;
mod guarddog;
mod malcontent;

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use scan::engine::{ScanResult, Scanner};
use serde_json::{Value, json};

use crate::bounded;
use clamav::ClamAv;
use guarddog::GuardDog;
use malcontent::Malcontent;

/// The scanners postdoc runs unless told otherwise. ClamAV is reached over a
/// Unix socket, and none of the three is deployed on other platforms.
#[cfg(unix)]
pub const DEFAULT: &str = "clamav,malcontent,guarddog";
/// The scanners postdoc runs unless told otherwise.
#[cfg(not(unix))]
pub const DEFAULT: &str = "none";

/// The most bytes of a scanner's error text an object carries: what scan
/// allows any free text, so the object stays inside hopper's limit.
const MAX_ERROR: usize = 400;

/// Which scanners to run and, where discovery will not do, where each is.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    /// The scanners to run.
    pub names: Vec<Scanner>,
    /// clamd's local socket. `None` reads it from clamd's configuration.
    pub clamav: Option<PathBuf>,
    /// The malcontent binary. `None` looks for `mal`.
    pub malcontent: Option<PathBuf>,
    /// The GuardDog binary. `None` looks for `guarddog`.
    pub guarddog: Option<PathBuf>,
}

/// `clamav,malcontent,guarddog`, any subset of them, or `none`.
///
/// # Errors
///
/// The list names something that is not one of the three.
pub fn parse_names(list: &str) -> Result<Vec<Scanner>> {
    let mut names = Vec::new();
    for name in list.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        let scanner = match name {
            "none" => continue,
            "clamav" => Scanner::Clamav,
            "malcontent" => Scanner::Malcontent,
            "guarddog" => Scanner::Guarddog,
            other => bail!("unknown scanner {other:?}; known: clamav, malcontent, guarddog, none"),
        };
        if !names.contains(&scanner) {
            names.push(scanner);
        }
    }
    Ok(names)
}

/// The scanners this process runs, each one checked at startup.
#[derive(Debug, Default)]
pub struct Scanners {
    clamav: Option<ClamAv>,
    malcontent: Option<Malcontent>,
    guarddog: Option<GuardDog>,
}

/// What a scanner reads: a file on disk, or bytes in memory with the name
/// they arrived under (malcontent types a file by its extension).
#[derive(Debug, Clone, Copy)]
pub enum Input<'a> {
    /// A file on disk.
    Path(&'a Path),
    /// Bytes in memory, and the artifact's name.
    Bytes {
        /// The artifact's bytes.
        bytes: &'a [u8],
        /// Its name, for its extension.
        name: &'a str,
    },
}

impl Scanners {
    /// Find and check every scanner `settings` names.
    ///
    /// Every one is tried before any failure is reported, so an operator
    /// fixing a host sees all that is wrong with it at once.
    ///
    /// # Errors
    ///
    /// A named scanner cannot be found, or does not answer.
    pub fn start(settings: Settings) -> Result<Self> {
        let Settings {
            names,
            clamav,
            malcontent,
            guarddog,
        } = settings;
        let mut scanners = Self::default();
        let mut problems = Vec::new();
        for name in names {
            let started = match name {
                Scanner::Clamav => ClamAv::start(clamav.clone()).map(|s| scanners.clamav = Some(s)),
                Scanner::Malcontent => {
                    Malcontent::start(malcontent.clone()).map(|s| scanners.malcontent = Some(s))
                }
                Scanner::Guarddog => {
                    GuardDog::start(guarddog.clone()).map(|s| scanners.guarddog = Some(s))
                }
                other => Err(anyhow::anyhow!("{other:?} is not a scanner postdoc runs")),
            };
            if let Err(e) = started {
                problems.push(format!("{e:#}"));
            }
        }
        if !problems.is_empty() {
            bail!(
                "{}\n\nInstall them with `make scanners` (or a postdoc deploy), name their \
                 locations with --clamav, --malcontent and --guarddog, or leave them out with \
                 --scanners (`--scanners none` runs none).",
                problems.join("\n")
            );
        }
        if scanners.is_empty() {
            tracing::warn!("no third-party scanners: results will carry atomdrift's jurors only");
        }
        Ok(scanners)
    }

    /// The switches that start the same scanners in another postdoc process,
    /// at the locations this one found: what the server hands its companion
    /// idle worker, which cannot inherit its command line.
    #[must_use]
    pub fn args(&self) -> Vec<OsString> {
        let mut names = Vec::new();
        let mut args = Vec::new();
        if let Some(clamd) = &self.clamav {
            names.push("clamav");
            args.extend([OsString::from("--clamav"), clamd.socket().into()]);
        }
        if let Some(mal) = &self.malcontent {
            names.push("malcontent");
            args.extend([OsString::from("--malcontent"), mal.bin().into()]);
        }
        if let Some(dog) = &self.guarddog {
            names.push("guarddog");
            args.extend([OsString::from("--guarddog"), dog.bin().into()]);
        }
        let list = if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(",")
        };
        [OsString::from("--scanners"), list.into()]
            .into_iter()
            .chain(args)
            .collect()
    }

    /// Whether no scanner runs.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.clamav.is_none() && self.malcontent.is_none() && self.guarddog.is_none()
    }

    /// Run every scanner on `input` and record each one's opinion on
    /// `result`'s upload. Nothing happens when the result was not classified
    /// for upload.
    pub fn run(&self, result: &mut ScanResult, input: Input<'_>) {
        if self.is_empty() {
            return;
        }
        let sha256 = result.sha256.clone();
        let file_type = result.file_type.clone();
        let Some(doc) = result.upload.as_mut() else {
            return;
        };
        let mut record = |scanner: Scanner, object: Value, started: Instant| {
            tracing::info!(sha256, ?scanner, elapsed_ms = scan::duration_ms(started.elapsed()), opinion = %object, "scanned");
            if let Err(e) = doc.set_scanner(scanner, object) {
                // The scanner ran, so it reports something: a failure, which
                // keeps its last finding.
                tracing::warn!(sha256, ?scanner, error = %e, "scanner opinion does not fit; recorded as an error");
                let _ = doc.set_scanner(
                    scanner,
                    json!({"outcome": "error", "error": format!("unrecordable opinion: {e}")}),
                );
            }
        };
        if let Some(clamd) = &self.clamav {
            let started = Instant::now();
            record(Scanner::Clamav, clamd.scan(input), started);
        }
        if let Some(mal) = &self.malcontent {
            let started = Instant::now();
            record(Scanner::Malcontent, mal.scan(input), started);
        }
        if let Some(dog) = &self.guarddog {
            let started = Instant::now();
            if let Some(object) = dog.scan(input, &file_type) {
                record(Scanner::Guarddog, object, started);
            }
        }
    }
}

/// A failed scan, with its error text cut to fit.
fn failed(outcome: &str, error: &str) -> Value {
    json!({"outcome": outcome, "error": bounded(error.trim(), MAX_ERROR)})
}

/// `name` on `PATH`, or in the places a deploy or a package manager puts
/// scanners when `PATH` is a service's narrow one.
fn find_program(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(["/usr/local/bin", "/opt/homebrew/bin", "/usr/bin"].map(PathBuf::from))
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// What a finished tool run left behind.
struct Ran {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Run `cmd` to completion, killing it past `timeout` and keeping at most
/// `limit` bytes of each of its outputs. A failure is the object to record
/// in place of an opinion.
fn run_tool(mut cmd: Command, name: &str, timeout: Duration, limit: u64) -> Result<Ran, Value> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| failed("error", &format!("running {name}: {e}")))?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(failed(
            "error",
            &format!("{name}'s output was not captured"),
        ));
    };
    let read = |pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut out = Vec::new();
            let _ = pipe.take(limit).read_to_end(&mut out);
            out
        })
    };
    let (stdout, stderr) = (read(Box::new(stdout)), read(Box::new(stderr)));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(failed(
                    "timeout",
                    &format!("{name} gave no answer in {}s", timeout.as_secs()),
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(failed("error", &format!("waiting for {name}: {e}"))),
        }
    };
    Ok(Ran {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// The last word of `bin --version`, the way each of these tools prints its
/// version (`malcontent version 1.28.1`, `guarddog, version 3.0.1`).
fn tool_version(bin: &Path) -> Result<String> {
    let out = Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| anyhow::anyhow!("running {} --version: {e}", bin.display()))?;
    let text = String::from_utf8_lossy(&out.stdout);
    match text.split_whitespace().last() {
        Some(version) if out.status.success() => Ok(version.to_owned()),
        _ => bail!(
            "{} --version said {:?} ({})",
            bin.display(),
            text.trim(),
            out.status
        ),
    }
}

/// The final component of `name`, so a path-shaped name cannot place a
/// staged file outside its directory.
fn file_name(name: &str) -> String {
    Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty() && n != "..")
        .unwrap_or_else(|| "artifact".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scanner_list_is_names_or_none() {
        assert_eq!(
            parse_names("clamav, guarddog,clamav").ok(),
            Some(vec![Scanner::Clamav, Scanner::Guarddog])
        );
        assert_eq!(parse_names("none").ok(), Some(Vec::new()));
        assert_eq!(parse_names("").ok(), Some(Vec::new()));
        assert!(parse_names("clamav,virustotal").is_err());
    }

    #[test]
    fn no_scanners_hands_a_child_none() {
        assert_eq!(
            Scanners::default().args(),
            vec![OsString::from("--scanners"), OsString::from("none")]
        );
    }

    #[test]
    fn a_scanner_that_cannot_start_names_itself_and_the_way_out() {
        let err = Scanners::start(Settings {
            names: vec![Scanner::Malcontent, Scanner::Guarddog],
            malcontent: Some(PathBuf::from("/nonexistent/mal")),
            guarddog: Some(PathBuf::from("/nonexistent/guarddog")),
            ..Settings::default()
        })
        .err()
        .map(|e| format!("{e:#}"))
        .unwrap_or_default();
        assert!(err.contains("malcontent"), "{err}");
        assert!(err.contains("guarddog"), "{err}");
        assert!(err.contains("--scanners"), "{err}");
    }

    #[test]
    fn a_staged_name_stays_in_its_directory() {
        assert_eq!(file_name("pkg/evil.tgz"), "evil.tgz");
        assert_eq!(file_name("../../etc/passwd"), "passwd");
        assert_eq!(file_name(".."), "artifact");
        assert_eq!(file_name(""), "artifact");
    }
}
