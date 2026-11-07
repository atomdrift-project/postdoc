//! How much memory a worker may size itself against, and the cgroup readings
//! that explain the answer.
//!
//! **The basis must be cgroup-aware.** `cleave::memory_tracker::total_memory()`
//! is the host's `MemTotal`, so on a shared host it hands back a budget covering
//! the whole machine. Admission control then fills that budget until the cgroup
//! OOM-kills the process. The worker's ceiling is therefore clamped to the
//! tightest limit on any ancestor cgroup.

use std::num::NonZeroU64;
// Paths are only walked where cgroups exist -- and under test, which exercises
// that walk against a temporary tree on every platform.
#[cfg(any(target_os = "linux", test))]
use std::path::{Path, PathBuf};

use scan::memory::MaxRssPolicy;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// Root of the cgroup-v2 hierarchy. A parameter in tests, a constant in life.
#[cfg(target_os = "linux")]
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// The worker's admission ceiling in bytes, resolved against this host; `None`
/// when throttling is disabled. `Auto` is 85% of the cgroup-aware
/// [`worker_memory_basis`], never below one GiB: slot count scales with cores,
/// so larger hosts need a proportionate ceiling.
pub(crate) fn worker_ceiling(policy: MaxRssPolicy) -> Option<NonZeroU64> {
    NonZeroU64::new(worker_max_rss_gb(policy).saturating_mul(GIB))
}

fn worker_max_rss_gb(policy: MaxRssPolicy) -> u64 {
    match policy {
        MaxRssPolicy::Disabled => 0,
        MaxRssPolicy::Auto => std::cmp::max(1, (worker_memory_basis().bytes * 85 / 100) / GIB),
        MaxRssPolicy::Explicit(gb) => gb.get(),
    }
}

/// Memory a process may size itself against, and where the number came from.
///
/// `source` is logged as `auto_memory_basis_source`, which is how an operator
/// confirms a cgroup limit was actually seen rather than assumed.
#[derive(Debug)]
pub(crate) struct WorkerMemoryBasis {
    /// Memory the process may size itself against.
    pub(crate) bytes: u64,
    /// Which signal supplied `bytes`, logged as `auto_memory_basis_source`.
    pub(crate) source: &'static str,
}

/// Smallest memory limit along a cgroup-v2 path, walking `start` up to `root`.
///
/// Split out from [`cgroup_memory_limit_bytes`] so the hierarchy walk is
/// testable without a real `/sys/fs/cgroup` -- which is why it is compiled
/// under `test` on every platform, and only on Linux otherwise.
#[cfg(any(target_os = "linux", test))]
fn cgroup_limit_under(root: &Path, start: &Path) -> Option<u64> {
    let mut dir = start.to_path_buf();
    let mut limit: Option<u64> = None;
    loop {
        for file in ["memory.max", "memory.high"] {
            if let Some(bytes) = memory_value_bytes(read_trimmed(dir.join(file)).as_deref()) {
                limit = Some(limit.map_or(bytes, |current: u64| current.min(bytes)));
            }
        }
        if dir == root || !dir.starts_with(root) {
            break;
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => break,
        }
    }
    limit
}

/// Most restrictive memory limit that applies to this process.
///
/// The limit that actually binds a process is the minimum over *every* ancestor
/// cgroup, not the value on its own. That distinction is load-bearing: a process
/// in a delegated child cgroup that carries no limit of its own finds nothing by
/// reading only [`cgroup_v2_path`], while the `MemoryMax=` on the parent unit is
/// exactly what OOM-kills it.
///
/// `memory.high` counts because a throttle the process can never outrun is a
/// ceiling in practice. Both files read `max` when unset, which
/// `memory_value_bytes` maps to `None`.
#[cfg(target_os = "linux")]
#[must_use]
fn cgroup_memory_limit_bytes() -> Option<u64> {
    cgroup_limit_under(Path::new(CGROUP_ROOT), &cgroup_v2_path()?)
}

/// No cgroups off Linux, so nothing clamps the host's memory here.
#[cfg(not(target_os = "linux"))]
#[must_use]
fn cgroup_memory_limit_bytes() -> Option<u64> {
    None
}

/// Memory this process may size itself against: host RAM, clamped to whatever
/// cgroup limit applies. See the module docs for why the clamp matters.
#[must_use]
pub(crate) fn worker_memory_basis() -> WorkerMemoryBasis {
    let host = cleave::memory_tracker::total_memory();
    let cgroup = cgroup_memory_limit_bytes();
    match (host, cgroup) {
        (Some(host), Some(cgroup)) if cgroup < host => WorkerMemoryBasis {
            bytes: cgroup,
            source: "cgroup_memory_limit",
        },
        (Some(host), _) => WorkerMemoryBasis {
            bytes: host,
            source: "cleave_total_memory",
        },
        (None, Some(cgroup)) => WorkerMemoryBasis {
            bytes: cgroup,
            source: "cgroup_memory_limit",
        },
        (None, None) => WorkerMemoryBasis {
            bytes: 16 * GIB,
            source: "fallback_16g",
        },
    }
}

/// Raw cgroup-v2 memory files for this process's own cgroup, for diagnostics.
#[derive(Debug, Default)]
pub(crate) struct CgroupMemoryDiagnostics {
    /// This process's cgroup-v2 directory.
    pub(crate) path: Option<String>,
    /// Raw `memory.current`.
    pub(crate) memory_current: Option<String>,
    /// `memory.current` in MiB.
    pub(crate) memory_current_mb: Option<u64>,
    /// Raw `memory.high` (`max` when unset).
    pub(crate) memory_high: Option<String>,
    /// `memory.high` in MiB, `None` when unset.
    pub(crate) memory_high_mb: Option<u64>,
    /// Raw `memory.max` (`max` when unset).
    pub(crate) memory_max: Option<String>,
    /// `memory.max` in MiB, `None` when unset.
    pub(crate) memory_max_mb: Option<u64>,
}

impl CgroupMemoryDiagnostics {
    /// Limit on *this* cgroup alone. [`cgroup_memory_limit_bytes`] is what
    /// actually binds the process; this is the local view, for logging.
    #[must_use]
    pub(crate) fn effective_limit_bytes(&self) -> Option<u64> {
        [self.memory_high.as_deref(), self.memory_max.as_deref()]
            .into_iter()
            .flatten()
            .filter_map(|v| memory_value_bytes(Some(v)))
            .min()
    }
}

/// Raw cgroup-v2 memory files for this process's own cgroup, for diagnostics.
#[cfg(target_os = "linux")]
#[must_use]
pub(crate) fn cgroup_memory_diagnostics() -> CgroupMemoryDiagnostics {
    let Some(path) = cgroup_v2_path() else {
        return CgroupMemoryDiagnostics::default();
    };
    let memory_current = read_trimmed(path.join("memory.current"));
    let memory_high = read_trimmed(path.join("memory.high"));
    let memory_max = read_trimmed(path.join("memory.max"));
    CgroupMemoryDiagnostics {
        path: Some(path.display().to_string()),
        memory_current_mb: memory_value_mb(memory_current.as_deref()),
        memory_high_mb: memory_value_mb(memory_high.as_deref()),
        memory_max_mb: memory_value_mb(memory_max.as_deref()),
        memory_current,
        memory_high,
        memory_max,
    }
}

/// Raw cgroup-v2 memory files for this process's own cgroup, for diagnostics.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub(crate) fn cgroup_memory_diagnostics() -> CgroupMemoryDiagnostics {
    CgroupMemoryDiagnostics::default()
}

/// This process's cgroup-v2 directory, from the unified hierarchy line.
#[cfg(target_os = "linux")]
#[must_use]
fn cgroup_v2_path() -> Option<PathBuf> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    for line in cgroup.lines() {
        let mut parts = line.splitn(3, ':');
        let hierarchy = parts.next()?;
        let controllers = parts.next()?;
        let rel = parts.next()?;
        if hierarchy == "0" && controllers.is_empty() {
            let rel = rel.trim_start_matches('/');
            return Some(if rel.is_empty() {
                PathBuf::from(CGROUP_ROOT)
            } else {
                PathBuf::from(CGROUP_ROOT).join(rel)
            });
        }
    }
    None
}

/// Contents of a cgroup file, trimmed, or `None` when absent or empty.
#[cfg(any(target_os = "linux", test))]
fn read_trimmed(path: PathBuf) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// A cgroup memory file's value in MiB, for the diagnostics log.
#[cfg(target_os = "linux")]
fn memory_value_mb(value: Option<&str>) -> Option<u64> {
    memory_value_bytes(value).map(|b| b / MIB)
}

/// Parse a cgroup memory file. `max` means "no limit", and like anything else
/// that is not a number maps to `None`.
fn memory_value_bytes(value: Option<&str>) -> Option<u64> {
    value?.parse().ok()
}

/// `MemTotal` from `/proc/meminfo`, in MiB.
pub(crate) fn proc_memtotal_mb() -> Result<u64, String> {
    let meminfo =
        std::fs::read_to_string("/proc/meminfo").map_err(|e| format!("read /proc/meminfo: {e}"))?;
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let raw = rest
                .split_whitespace()
                .next()
                .ok_or_else(|| "parse /proc/meminfo MemTotal: missing value".to_string())?;
            let kb: u64 = raw
                .parse()
                .map_err(|e| format!("parse /proc/meminfo MemTotal value {raw:?}: {e}"))?;
            return Ok(kb / 1024);
        }
    }
    Err("parse /proc/meminfo: MemTotal not found".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A disposable cgroup-shaped directory tree.
    struct TempTree(tempfile::TempDir);

    impl TempTree {
        fn new() -> Self {
            Self(tempfile::tempdir().expect("create temp tree"))
        }

        fn write(&self, rel: &str, file: &str, value: &str) {
            let dir = self.path(rel);
            std::fs::create_dir_all(&dir).expect("create cgroup dir");
            std::fs::write(dir.join(file), value).expect("write cgroup file");
        }

        /// `rel` under the tree; empty is the root itself (`join("")` would
        /// add a trailing separator).
        fn path(&self, rel: &str) -> PathBuf {
            if rel.is_empty() {
                self.0.path().to_owned()
            } else {
                self.0.path().join(rel)
            }
        }
    }

    #[test]
    fn limit_on_a_parent_binds_a_child_that_has_none() {
        // The exact shape that defeated a non-hierarchical read: the delegated
        // `scan.service/idle` cgroup carries no limit, the unit above it does.
        let tree = TempTree::new();
        tree.write("scan.service", "memory.max", "128849018880");
        tree.write("scan.service/idle", "memory.max", "max");
        let got = cgroup_limit_under(&tree.path(""), &tree.path("scan.service/idle"));
        assert_eq!(got, Some(128_849_018_880));
    }

    #[test]
    fn the_most_restrictive_ancestor_wins() {
        let tree = TempTree::new();
        tree.write("", "memory.max", "100");
        tree.write("a", "memory.max", "50");
        tree.write("a/b", "memory.max", "900");
        assert_eq!(
            cgroup_limit_under(&tree.path(""), &tree.path("a/b")),
            Some(50)
        );
    }

    #[test]
    fn memory_high_is_a_ceiling_too() {
        let tree = TempTree::new();
        tree.write("a", "memory.max", "900");
        tree.write("a", "memory.high", "17");
        assert_eq!(
            cgroup_limit_under(&tree.path(""), &tree.path("a")),
            Some(17)
        );
    }

    #[test]
    fn unlimited_everywhere_is_no_limit() {
        let tree = TempTree::new();
        tree.write("", "memory.max", "max");
        tree.write("a", "memory.max", "max");
        assert_eq!(cgroup_limit_under(&tree.path(""), &tree.path("a")), None);
    }

    #[test]
    fn a_cgroup_limit_below_host_ram_becomes_the_basis() {
        // worker_memory_basis reads the real host, so assert the property that
        // matters rather than a number: whichever source wins is labelled.
        let basis = worker_memory_basis();
        assert!(basis.bytes > 0);
        assert!(matches!(
            basis.source,
            "cgroup_memory_limit" | "cleave_total_memory" | "fallback_16g"
        ));
    }

    #[test]
    fn zero_resolves_a_real_ceiling_rather_than_meaning_unlimited() {
        // The collision: a front end passing raw 0 into Startup would disable
        // admission. Resolved, 0 must yield a positive ceiling.
        assert!(worker_max_rss_gb(MaxRssPolicy::from_cli(0)) > 0);
        assert!(MaxRssPolicy::from_cli(0).process_ceiling().is_some());
    }

    #[test]
    fn negative_disables_and_explicit_is_verbatim() {
        assert_eq!(worker_max_rss_gb(MaxRssPolicy::from_cli(-1)), 0);
        assert_eq!(MaxRssPolicy::from_cli(-1).process_ceiling(), None);
        assert_eq!(worker_max_rss_gb(MaxRssPolicy::from_cli(3)), 3);
        assert_eq!(
            MaxRssPolicy::from_cli(3)
                .process_ceiling()
                .map(NonZeroU64::get),
            Some(3 * GIB)
        );
    }

    #[test]
    fn auto_ceiling_never_exceeds_the_cgroup_limit() {
        if let Some(limit) = cgroup_memory_limit_bytes() {
            let resolved_gb = worker_max_rss_gb(MaxRssPolicy::from_cli(0));
            assert!(
                resolved_gb.saturating_mul(GIB) <= limit,
                "resolved {resolved_gb} GiB exceeds cgroup limit {limit} bytes",
            );
        }
    }
}
