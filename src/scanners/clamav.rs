//! ClamAV, through clamd on its local socket.
//!
//! The bytes are streamed (`INSTREAM`) rather than named by path, so the
//! daemon needs no access to this process's files and the same call serves a
//! download held in memory. clamd keeps its signature database loaded, which
//! `clamscan` would reload, for tens of seconds, on every sample.

use std::path::PathBuf;

use anyhow::Result;
use serde_json::{Value, json};

use super::{MAX_ERROR, failed};
use crate::bounded;

/// clamd configurations, in the places each platform's package puts them.
const CONFIGS: [&str; 6] = [
    "/etc/clamav/clamd.conf",
    "/etc/clamd.d/scan.conf",
    "/etc/clamd.conf",
    "/usr/local/etc/clamd.conf",
    "/opt/homebrew/etc/clamav/clamd.conf",
    "/usr/local/etc/clamav/clamd.conf",
];

/// Where packaged clamds put their socket, for a configuration that is not
/// in one of [`CONFIGS`].
const SOCKETS: [&str; 5] = [
    "/run/clamav/clamd.ctl",
    "/var/run/clamav/clamd.ctl",
    "/run/clamd.scan/clamd.sock",
    "/var/run/clamav/clamd.sock",
    "/opt/homebrew/var/run/clamav/clamd.sock",
];

/// clamd, spoken to over its local socket.
#[derive(Debug)]
pub(super) struct ClamAv {
    socket: PathBuf,
}

impl ClamAv {
    /// clamd at `socket`, or at the socket its configuration names, asked
    /// its version to prove it answers.
    pub(super) fn start(socket: Option<PathBuf>) -> Result<Self> {
        let socket = match socket {
            Some(socket) => socket,
            None => find_socket().ok_or_else(|| {
                anyhow::anyhow!(
                    "clamav: no clamd socket: no clamd.conf in {} names a LocalSocket that \
                     exists, and none of {} does (is clamd installed and running?)",
                    CONFIGS.join(", "),
                    SOCKETS.join(", ")
                )
            })?,
        };
        let clamd = Self { socket };
        let (engine, db) = clamd.version().map_err(|e| {
            anyhow::anyhow!(
                "clamav: clamd at {} does not answer: {e}",
                clamd.socket.display()
            )
        })?;
        tracing::info!(socket = %clamd.socket.display(), engine, db, "clamav ready");
        Ok(clamd)
    }

    pub(super) fn socket(&self) -> &std::path::Path {
        &self.socket
    }
}

/// The first configured or conventional clamd socket that exists.
fn find_socket() -> Option<PathBuf> {
    CONFIGS
        .iter()
        .filter_map(|conf| std::fs::read_to_string(conf).ok())
        .filter_map(|text| local_socket(&text))
        .chain(SOCKETS.iter().map(PathBuf::from))
        .find(|socket| socket.exists())
}

/// The `LocalSocket` a clamd configuration sets, if it sets one.
fn local_socket(conf: &str) -> Option<PathBuf> {
    conf.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        let key = words.next()?;
        let value = words.next()?;
        key.eq_ignore_ascii_case("LocalSocket")
            .then(|| PathBuf::from(value))
    })
}

#[cfg(unix)]
mod wire {
    use std::io::{self, Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    use serde_json::Value;

    use super::{ClamAv, clamd_opinion, parse_version};
    use crate::scanners::{Input, failed};

    /// Longest a clamd exchange may go without progress. A stalled daemon
    /// must not hold an analysis slot forever.
    const CLAMD_TIMEOUT: Duration = Duration::from_secs(300);

    /// INSTREAM chunk size. clamd takes any size up to its
    /// `StreamMaxLength`; this only bounds the buffer.
    pub(super) const CLAMD_CHUNK: usize = 64 * 1024;

    impl ClamAv {
        fn connect(&self) -> io::Result<UnixStream> {
            let stream = UnixStream::connect(&self.socket)?;
            stream.set_read_timeout(Some(CLAMD_TIMEOUT))?;
            stream.set_write_timeout(Some(CLAMD_TIMEOUT))?;
            Ok(stream)
        }

        /// clamd's engine version and signature database version.
        pub(super) fn version(&self) -> io::Result<(String, String)> {
            let mut stream = self.connect()?;
            stream.write_all(b"zVERSION\0")?;
            let reply = read_reply(&mut stream)?;
            parse_version(&reply).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unexpected VERSION reply {reply:?}"),
                )
            })
        }

        pub(in crate::scanners) fn scan(&self, input: Input<'_>) -> Value {
            // Asked every time: the database changes under a running daemon.
            let stamp = self.version();
            let reply = match self.instream(input) {
                Ok(reply) => reply,
                Err(e) if is_timeout(&e) => return failed("timeout", &e.to_string()),
                Err(e) => return failed("error", &format!("clamd: {e}")),
            };
            let mut object = clamd_opinion(&reply);
            if let (Ok((engine, db)), Some(fields)) = (stamp, object.as_object_mut()) {
                fields.insert("engine".to_owned(), engine.into());
                fields.insert("db".to_owned(), db.into());
            }
            object
        }

        fn instream(&self, input: Input<'_>) -> io::Result<String> {
            let mut stream = self.connect()?;
            let sent = send_stream(&mut stream, input);
            // A daemon that refuses the stream part-way (over
            // `StreamMaxLength`) replies and closes, so its reply explains a
            // failed write.
            match (sent, read_reply(&mut stream)) {
                (_, Ok(reply)) if !reply.is_empty() => Ok(reply),
                (Err(e), _) | (Ok(()), Err(e)) => Err(e),
                (Ok(()), Ok(_)) => Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "clamd closed without a reply",
                )),
            }
        }
    }

    /// `input` as one clamd `INSTREAM` command: length-prefixed chunks, then
    /// a zero-length one.
    fn send_stream(stream: &mut UnixStream, input: Input<'_>) -> io::Result<()> {
        stream.write_all(b"zINSTREAM\0")?;
        let mut send = |chunk: &[u8]| -> io::Result<()> {
            let len = u32::try_from(chunk.len())
                .map_err(|_too_long| io::Error::other("chunk over 4 GiB"))?;
            stream.write_all(&len.to_be_bytes())?;
            stream.write_all(chunk)
        };
        match input {
            Input::Bytes { bytes, .. } => {
                for chunk in bytes.chunks(CLAMD_CHUNK) {
                    send(chunk)?;
                }
            }
            Input::Path(path) => {
                let mut file = std::fs::File::open(path)?;
                let mut buf = vec![0; CLAMD_CHUNK];
                loop {
                    let n = file.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    send(buf.get(..n).unwrap_or_default())?;
                }
            }
        }
        send(&[])
    }

    /// One NUL-terminated clamd reply, without the terminator.
    fn read_reply(stream: &mut UnixStream) -> io::Result<String> {
        let mut reply = Vec::new();
        stream.take(64 * 1024).read_to_end(&mut reply)?;
        if let Some(end) = reply.iter().position(|&b| b == 0) {
            reply.truncate(end);
        }
        Ok(String::from_utf8_lossy(&reply).trim().to_owned())
    }

    fn is_timeout(e: &io::Error) -> bool {
        matches!(
            e.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        )
    }
}

#[cfg(not(unix))]
impl ClamAv {
    fn version(&self) -> std::io::Result<(String, String)> {
        Err(std::io::Error::other(
            "clamd is reached over a Unix socket, which this platform does not have",
        ))
    }

    pub(super) fn scan(&self, _input: super::Input<'_>) -> Value {
        failed("unsupported", "clamd is not reachable on this platform")
    }
}

/// `ClamAV 1.4.1/28044/Thu Nov 12 09:24:02 2026` as its engine and database
/// versions.
fn parse_version(reply: &str) -> Option<(String, String)> {
    let mut parts = reply.strip_prefix("ClamAV ")?.split('/');
    let engine = parts.next()?.trim();
    let db = parts.next().map_or("", str::trim);
    (!engine.is_empty()).then(|| (engine.to_owned(), db.to_owned()))
}

/// What clamd's reply to a stream says, as an opinion.
///
/// A signature is hostile unless it is ClamAV's own hedge: `PUA.` names
/// software that is unwanted rather than malicious, and `Heuristics.` a
/// pattern rather than a known sample, so both are suspicious. A limit
/// reached is no reading at all.
fn clamd_opinion(reply: &str) -> Value {
    let body = reply.strip_prefix("stream:").map_or(reply, str::trim);
    if body == "OK" {
        return json!({"class": "benign"});
    }
    if let Some(signature) = body.strip_suffix(" FOUND").map(str::trim) {
        if signature.starts_with("Heuristics.Limits.Exceeded") {
            return failed("unsupported", &format!("scan limit reached: {signature}"));
        }
        let class = if signature.starts_with("PUA.") || signature.starts_with("Heuristics.") {
            "suspicious"
        } else {
            "hostile"
        };
        return json!({"class": class, "signature": bounded(signature, MAX_ERROR)});
    }
    if body.contains("size limit exceeded") {
        return failed("unsupported", body);
    }
    failed("error", &format!("clamd: {body}"))
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::io::{Read, Write};

    use super::*;
    use crate::scanners::Input;

    #[test]
    fn clamd_versions_are_read_from_its_banner() {
        assert_eq!(
            parse_version("ClamAV 1.4.1/28044/Thu Nov 12 09:24:02 2026"),
            Some(("1.4.1".to_owned(), "28044".to_owned()))
        );
        assert_eq!(
            parse_version("ClamAV 1.4.1"),
            Some(("1.4.1".to_owned(), String::new()))
        );
        assert_eq!(parse_version("UNKNOWN COMMAND"), None);
    }

    #[test]
    fn the_socket_is_read_from_clamds_configuration() {
        let conf = "# LocalSocket /tmp/commented.sock\nExample\nLocalSocket /run/clamav/clamd.ctl\nFixStaleSocket true\n";
        assert_eq!(
            local_socket(conf),
            Some(PathBuf::from("/run/clamav/clamd.ctl"))
        );
        assert_eq!(local_socket("#LocalSocket /x\nTCPSocket 3310\n"), None);
    }

    #[test]
    fn clamd_replies_become_opinions() {
        assert_eq!(clamd_opinion("stream: OK"), json!({"class": "benign"}));
        assert_eq!(
            clamd_opinion("stream: Js.Trojan.Agent-1 FOUND"),
            json!({"class": "hostile", "signature": "Js.Trojan.Agent-1"})
        );
        assert_eq!(
            clamd_opinion("stream: PUA.Win.Packer.Upx-1 FOUND")["class"],
            "suspicious"
        );
        assert_eq!(
            clamd_opinion("stream: Heuristics.Encrypted.Zip FOUND")["class"],
            "suspicious"
        );
        assert_eq!(
            clamd_opinion("stream: Heuristics.Limits.Exceeded.MaxFileSize FOUND")["outcome"],
            "unsupported",
            "a limit reached is no reading"
        );
        assert_eq!(
            clamd_opinion("INSTREAM size limit exceeded. ERROR")["outcome"],
            "unsupported"
        );
        assert_eq!(
            clamd_opinion("stream: Can't allocate memory ERROR")["outcome"],
            "error"
        );
    }

    /// A stand-in clamd on a socket: it answers `VERSION` every time it is
    /// asked, and an `INSTREAM` with `reply` once it has read every chunk.
    fn fake_clamd(
        reply: &'static str,
        connections: usize,
    ) -> (tempfile::TempDir, PathBuf, std::thread::JoinHandle<Vec<u8>>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("clamd.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
        let served = std::thread::spawn(move || {
            let mut streamed = Vec::new();
            for _ in 0..connections {
                let (mut conn, _) = listener.accept().expect("accept");
                let mut command = Vec::new();
                let mut byte = [0u8; 1];
                while conn.read_exact(&mut byte).is_ok() && byte[0] != 0 {
                    command.push(byte[0]);
                }
                if command == b"zVERSION" {
                    conn.write_all(b"ClamAV 1.5.4/28044/Sat Jun 27 02:24:28 2026\0")
                        .expect("reply");
                    continue;
                }
                assert_eq!(command, b"zINSTREAM");
                loop {
                    let mut len = [0u8; 4];
                    conn.read_exact(&mut len).expect("chunk length");
                    let len = u32::from_be_bytes(len) as usize;
                    if len == 0 {
                        break;
                    }
                    let mut chunk = vec![0; len];
                    conn.read_exact(&mut chunk).expect("chunk");
                    streamed.extend(chunk);
                }
                conn.write_all(format!("{reply}\0").as_bytes())
                    .expect("reply");
            }
            streamed
        });
        (dir, socket, served)
    }

    #[test]
    fn clamd_is_checked_at_start_then_streamed_the_bytes() {
        let (_dir, socket, served) = fake_clamd("stream: Eicar-Test-Signature FOUND", 3);
        let clamd = ClamAv::start(Some(socket)).expect("the stand-in answers VERSION");
        let bytes = vec![7u8; wire::CLAMD_CHUNK * 2 + 5];
        let opinion = clamd.scan(Input::Bytes {
            bytes: &bytes,
            name: "x.com",
        });
        assert_eq!(
            opinion,
            json!({"class": "hostile", "signature": "Eicar-Test-Signature", "engine": "1.5.4", "db": "28044"})
        );
        assert_eq!(
            served.join().expect("fake clamd"),
            bytes,
            "every chunk, in order"
        );
    }

    #[test]
    fn a_silent_clamd_stops_the_start_and_fails_a_scan() {
        let missing = PathBuf::from("/nonexistent/clamd.sock");
        let err = ClamAv::start(Some(missing.clone()))
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(err.contains("does not answer"), "{err}");
        let clamd = ClamAv { socket: missing };
        assert_eq!(
            clamd.scan(Input::Bytes {
                bytes: b"x",
                name: "x"
            })["outcome"],
            "error"
        );
    }
}
