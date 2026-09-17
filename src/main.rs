//! postdoc — worker and server coordination for the Atomdrift analysis fleet.
//!
//! Two modes. `worker` claims artifacts from hopper, judges them, and posts
//! results back. `serve` answers beamline. Both are long-lived daemons; there
//! is no one-shot mode, because `atomscan` and `isomer` already are one.

/// jemalloc, plus the compile-time tuning it reads at initialization.
///
/// A `#[global_allocator]` static has to be declared in the binary crate, so
/// this cannot live in a library and every binary running cleave's analysis
/// declares its own. The tuning string does not: it is
/// [`cleave::JEMALLOC_CONF`], shared with `atomscan`, so the two allocate
/// alike. See that constant for what each option buys.
///
/// Running this workload on the system allocator is not a small difference.
/// Rayon workers convoy on the process heap, and on macOS the default
/// small-object zone retains freed pages — measured at ~0.8 GB still held at a
/// peak. A worker whose memory behaviour diverges from the one the fleet was
/// sized against is a worker that gets OOM-killed on a host the old one
/// survived.
///
/// Allocator and configuration share one `cfg` so they cannot drift apart: a
/// build that swaps in the system allocator must not leave a
/// `_rjem_malloc_conf` symbol behind, and one that uses jemalloc must never be
/// left unconfigured. On the excluded targets this crate uses the system
/// allocator; on FreeBSD that *is* jemalloc, but it reads the unprefixed
/// `MALLOC_CONF`, so this symbol would not reach it anyway.
#[cfg(all(
    unix,
    not(any(
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "illumos",
        target_os = "solaris",
    ))
))]
// A binary that owns its process has to configure the allocator, and that is
// unsafe by construction. Scoped here rather than relaxed crate-wide: no other
// module in postdoc has any business writing unsafe.
#[allow(unsafe_code)]
mod jemalloc {
    #[global_allocator]
    static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

    /// A `Sync` wrapper so a raw `*const c_char` can live in a static;
    /// jemalloc reads the pointer, it is never written after link time.
    #[repr(transparent)]
    struct SyncPtr(*const std::os::raw::c_char);
    // SAFETY: the pointer targets a NUL-terminated 'static CStr and is never
    // mutated, so sharing it across threads is sound.
    unsafe impl Sync for SyncPtr {}

    #[allow(non_upper_case_globals)]
    #[unsafe(no_mangle)]
    static _rjem_malloc_conf: SyncPtr = SyncPtr(cleave::JEMALLOC_CONF.as_ptr());

    /// Route tree-sitter's C-core allocations through jemalloc.
    ///
    /// Its parse trees otherwise go to the system malloc — outside every
    /// jemalloc budget, decay policy, and heap profile this stack relies on.
    ///
    /// Installs via the tree-sitter crate's [`tree_sitter::set_allocator`]
    /// rather than raw `ts_set_allocator`: the crate keeps an internal free-fn
    /// for C strings it releases, and bypassing the wrapper would free
    /// jemalloc pointers with libc `free`.
    ///
    /// # Safety
    ///
    /// Inherits [`tree_sitter::set_allocator`]'s contract: no tree-sitter API
    /// may have run, no tree-sitter object may be live, and no other thread
    /// may be in tree-sitter concurrently. In practice: call once, first thing
    /// in `main`.
    pub(super) unsafe fn route_tree_sitter_through_jemalloc() {
        // SAFETY: jemalloc's malloc/calloc/realloc/free are one allocator
        // family, never return null for non-zero sizes, and satisfy libc
        // malloc alignment. The ordering and thread-exclusivity clauses are
        // this function's own documented precondition.
        unsafe {
            tree_sitter::set_allocator(Some(tree_sitter::Allocator {
                malloc: tikv_jemalloc_sys::malloc,
                calloc: tikv_jemalloc_sys::calloc,
                realloc: tikv_jemalloc_sys::realloc,
                free: tikv_jemalloc_sys::free,
            }));
        }
    }
}

use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// Operational error. A worker that cannot start says why and stops, rather
/// than idling in a loop that will never claim anything.
const EXIT_ERROR: u8 = 1;

/// Settings both daemons take.
///
/// Declared once and flattened into each mode rather than written twice. A
/// flag defined in two places is a flag whose two definitions will eventually
/// disagree — a different default, a different unit, a different help string —
/// and the disagreement shows up as a host behaving unlike its neighbour.
#[derive(Debug, clap::Args)]
struct DaemonArgs {
    /// Concurrent analysis slots. Default: three per physical core.
    ///
    /// Slots outnumber cores because a job spends much of its life waiting on
    /// the network, not on CPU.
    #[arg(short = 'j', long, global = true)]
    workers: Option<NonZeroUsize>,

    /// RSS ceiling in gigabytes. `0` resolves one from the host; `-1` disables
    /// in-process throttling, for when an external supervisor enforces a cap.
    ///
    /// Deployment does not pass this. The default resolves a ceiling well
    /// below any cgroup limit, so postdoc sheds work before the kernel sheds
    /// postdoc.
    #[arg(long, global = true, allow_hyphen_values = true)]
    max_rss_gb: Option<i64>,

    /// Traits bundle override.
    #[arg(long, global = true)]
    traits_dir: Option<PathBuf>,
}

/// Worker and server coordination for the Atomdrift analysis fleet.
#[derive(Debug, Parser)]
#[command(version, about, max_term_width = 100)]
struct Cli {
    /// Scan's own global flags, flattened rather than restated.
    ///
    /// `--llm*`, `--fetch*`, `--follow`, `--threshold-*`, `-l`, `--mode`,
    /// `--zip-password` and the rest all reach the analysis stack here. They
    /// are declared once, in scan, so `postdoc serve --llm …` cannot come to
    /// mean something different from `atomscan serve --llm …` — which is what
    /// restating thirty-eight flags in a second binary would eventually do.
    #[command(flatten)]
    global: scan::cli::GlobalArgs,

    #[command(flatten)]
    daemon: DaemonArgs,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Poll a hopper instance for analysis jobs and post back verdicts.
    ///
    /// Accepts the arguments `atomscan worker` accepts, so a deployment that
    /// supervises one supervises the other by changing the binary name and
    /// nothing else.
    Worker {
        /// Hopper API base URL (e.g. http://hopper-host:8081). Every call
        /// authenticates with `~/.tok/hopper` (or `$HOPPER_TOKEN_FILE` /
        /// `$HOPPER_TOKEN`); without it hopper rejects the poll with 401.
        ///
        /// Accepts the comma list `serve --hopper` takes, so one deploy
        /// variable can feed both, but a worker uses only the primary — the
        /// last address. A replica refuses worker routes outright, so the
        /// earlier ones are not a fallback here.
        #[arg(long, env = "SCAN_HOPPER")]
        url: String,

        /// Worker name (defaults to hostname).
        #[arg(long)]
        name: Option<String>,

        /// Seconds to wait before polling again when hopper has no work.
        #[arg(long, default_value = "2")]
        poll_secs: u64,

        /// A sample tree this worker can read directly, skipping the download.
        #[arg(long)]
        data_dir: Option<PathBuf>,

        /// Stop after this many jobs.
        #[arg(long)]
        max_jobs: Option<u64>,

        /// Nice value for the analysis threads.
        #[arg(long, default_value = "18", allow_hyphen_values = true)]
        nice: i32,

        /// Exit rather than idle when hopper has nothing to claim.
        #[arg(long)]
        exit_if_empty: bool,

        /// Skip the trait-validation gate.
        ///
        /// A worker running an incomplete rule set reports benign verdicts it
        /// has not earned, and does it quietly. For local work against
        /// on-disk rules, never for a fleet.
        #[arg(long)]
        no_validate: bool,
    },

    /// Serve the HTTP analysis API.
    ///
    /// A drop-in for `atomscan serve`: the same routes, answered the same
    /// way, so swapping the binary is a deployment change and nothing else.
    Serve {
        /// Address to listen on.
        #[arg(long, default_value = "127.0.0.1:49999")]
        bind: SocketAddr,

        /// Largest request body accepted, in megabytes.
        #[arg(long, default_value = "100")]
        max_size_mb: usize,

        /// Comma-separated directories `/analyze-path` may read. Empty means
        /// that route refuses every request.
        #[arg(long)]
        allowed_dirs: Option<String>,

        /// Where archive members are extracted for callers to fetch.
        #[arg(long)]
        extract_dir: Option<PathBuf>,

        /// Comma-separated CIDRs allowed to reach non-loopback routes.
        #[arg(long)]
        allow_cidr: Option<String>,

        /// File holding the bearer token required on every route but
        /// `/_/health`. Omitted disables authentication.
        #[arg(long, value_name = "PATH")]
        token_file: Option<PathBuf>,

        /// Hopper API root. Enables result renewal, corpus deferral for
        /// unknown lookups, and the companion idle worker.
        #[arg(long, value_name = "URL", env = "SCAN_HOPPER")]
        hopper: Option<String>,

        /// Slots for the companion idle worker. Defaults to half the request
        /// slots; ignored without `--hopper`.
        #[arg(long, value_name = "N", env = "SCAN_IDLE_WORKER_SLOTS")]
        idle_worker_slots: Option<usize>,

        /// Per-request analysis timeout in seconds. `0` disables.
        #[arg(long, default_value_t = scan::server::DEFAULT_ANALYSIS_TIMEOUT_SECS)]
        analysis_timeout: u64,
    },
}

/// Windows counterpart of [`jemalloc`].
///
/// The CRT heap was measured 12% exclusive on a two-Go profile, with rayon
/// workers convoying on it — the same contention jemalloc is chosen to avoid
/// elsewhere. Route tree-sitter's C allocator through the same arena so parse
/// trees are not a second heap.
#[cfg(windows)]
// Same reasoning as `mod jemalloc`: a binary that owns its process has to
// configure the allocator, and that is unsafe by construction.
#[allow(unsafe_code)]
mod mimalloc_alloc {
    #[global_allocator]
    static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

    /// mimalloc exposes no `calloc`, so zeroing is done here.
    ///
    /// # Safety
    ///
    /// Same contract as [`super::jemalloc::route_tree_sitter_through_jemalloc`]:
    /// call once, first thing in `main`, before any tree-sitter API.
    unsafe extern "C" fn calloc_compat(count: usize, size: usize) -> *mut std::ffi::c_void {
        let Some(bytes) = count.checked_mul(size) else {
            return std::ptr::null_mut();
        };
        // SAFETY: `mi_malloc` is mimalloc's own allocation entry point.
        let ptr = unsafe { libmimalloc_sys::mi_malloc(bytes) };
        if !ptr.is_null() && bytes > 0 {
            // SAFETY: the allocation above succeeded and is `bytes` long.
            unsafe { std::ptr::write_bytes(ptr.cast::<u8>(), 0, bytes) };
        }
        ptr
    }

    /// Route tree-sitter's C-core allocations through mimalloc.
    ///
    /// # Safety
    ///
    /// Inherits [`tree_sitter::set_allocator`]'s contract: call once, first
    /// thing in `main`, before any tree-sitter API and before any thread.
    pub(super) unsafe fn route_tree_sitter_through_mimalloc() {
        // SAFETY: mimalloc's entry points are one allocator family, and the
        // ordering clauses are this function's documented precondition.
        unsafe {
            tree_sitter::set_allocator(Some(tree_sitter::Allocator {
                malloc: libmimalloc_sys::mi_malloc,
                calloc: calloc_compat,
                realloc: libmimalloc_sys::mi_realloc,
                free: libmimalloc_sys::mi_free,
            }));
        }
    }
}

#[allow(unsafe_code)] // see `mod jemalloc`
fn main() -> ExitCode {
    // SAFETY: the first statements of `main`. No tree-sitter API has run, no
    // tree-sitter object is live, no thread has been spawned, and nothing has
    // read the environment yet.
    #[cfg(all(
        unix,
        not(any(
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "illumos",
            target_os = "solaris",
        ))
    ))]
    unsafe {
        jemalloc::route_tree_sitter_through_jemalloc();
    }
    // SAFETY: as above.
    #[cfg(windows)]
    unsafe {
        mimalloc_alloc::route_tree_sitter_through_mimalloc();
    }
    // SAFETY: as above — still before any thread or environment read.
    unsafe { scan::runtime::install() };

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("postdoc=info,scan=info")),
        )
        .init();

    // Crash and thread dumps: a wedged worker has to be diagnosable in place,
    // and this must follow the signal mask `runtime::install` set.
    scan::runtime::install_diagnostics();
    // Reclaim stale and oversized caches on a recurring loop, as a
    // never-exiting daemon needs.
    scan::cache_cleanup::start(true);
    // Stabilize trait discovery before any cleave shared resource initializes.
    scan::traits_repo::prepare_runtime_env();

    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("postdoc: {error:#}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let command = cli.command;
    match command {
        Command::Worker {
            url,
            name,
            poll_secs,
            data_dir,
            max_jobs,
            nice,
            exit_if_empty,
            no_validate,
        } => {
            // Every default is settled by scan's own resolver, so a postdoc
            // worker and an atomscan worker start on identical footing — the
            // same model bundle, operating point, slot count, memory ceiling
            // and validation gate.
            let config = scan::worker::Startup {
                hopper_url: url,
                name,
                workers: cli.daemon.workers,
                poll_secs,
                max_rss_gb: cli.daemon.max_rss_gb.unwrap_or(0),
                nice,
                data_dir,
                max_jobs,
                exit_if_empty,
                traits_dir: cli.daemon.traits_dir.clone(),
                update: cli.global.update,
                model_dir: cli.global.model_dir.clone(),
                // Both are global flags, so a worker has to honor them the way
                // atomscan's does. Left unset they stay the bundle's choice,
                // which is what a fleet worker normally runs on.
                level: cli.global.level,
                thresholds: cli.global.thresholds(),
                no_update: cli.global.no_update,
                no_validate,
                slow_rule_ms: scan::cli::DEFAULT_SLOW_RULE_MS,
                interpret: cli.global.interpret_config()?,
                // A worker populates the shared corpus rather than answering
                // one question quickly, so it takes every resolvable
                // dependency: the fresh-risk window that keeps an interactive
                // scan fast discards exactly the long tail the cache wants.
                fetch: cli.global.fetch_policy(
                    cli.global
                        .follow
                        .unwrap_or_else(scan::fetch::default_service_follow_policy),
                    scan::fetch::WORKER_MAX_DEP_AGE_DAYS,
                    true,
                ),
                zip_passwords: cli.global.zip_passwords.clone().into(),
            }
            .resolve()?;

            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(scan::worker::run(config))
        }

        Command::Serve {
            bind,
            max_size_mb,
            allowed_dirs,
            extract_dir,
            allow_cidr,
            token_file,
            hopper,
            idle_worker_slots,
            analysis_timeout,
        } => {
            // Hopper drives three things at once here: result renewal, the
            // corpus deferral for unknown lookups, and whether the companion
            // idle worker has anything to claim.
            let hopper = hopper.filter(|url| !url.trim().is_empty());
            let config = scan::server::Startup {
                bind,
                max_size_mb,
                max_rss_gb: cli.daemon.max_rss_gb.unwrap_or(0),
                allowed_dirs,
                extract_dir,
                workers: cli.daemon.workers,
                allow_cidr,
                token_file,
                traits_dir: cli.daemon.traits_dir.clone(),
                update: cli.global.update,
                no_update: cli.global.no_update,
                hopper,
                idle_worker_slots,
                analysis_timeout_secs: analysis_timeout,
                model_dir: cli.global.model_dir.clone(),
                level: cli.global.level,
                thresholds: cli.global.thresholds(),
                slow_rule_ms: scan::cli::DEFAULT_SLOW_RULE_MS,
                interpret: cli.global.interpret_config()?,
                fetch: cli.global.fetch_policy(
                    cli.global
                        .follow
                        .unwrap_or_else(scan::fetch::default_service_follow_policy),
                    scan::fetch::DEFAULT_MAX_DEP_AGE_DAYS,
                    true,
                ),
                zip_passwords: cli.global.zip_passwords.clone().into(),
            }
            .resolve()?;

            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(scan::server::run(config))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn a_worker_needs_only_a_hopper_url() {
        let cli = Cli::try_parse_from(["postdoc", "worker", "--url", "http://h:8081"]).unwrap();
        let Command::Worker {
            url,
            poll_secs,
            nice,
            ..
        } = cli.command
        else {
            panic!("`worker` must parse as the worker command");
        };
        assert_eq!(url, "http://h:8081");
        // Deployment passes none of these; the binary states them once.
        assert!(
            cli.daemon.max_rss_gb.is_none(),
            "unset means postdoc resolves its own ceiling"
        );
        assert!(cli.daemon.workers.is_none(), "three per physical core");
        assert_eq!(poll_secs, 2);
        assert_eq!(nice, 18);
    }

    #[test]
    fn the_argv_hopper_spawns_a_local_worker_with_is_accepted() {
        // Hopper supervises its own worker with exactly this command line. If
        // postdoc cannot parse it, the binary is not a drop-in and the
        // cutover is a config change rather than a rename.
        let cli = Cli::try_parse_from([
            "postdoc",
            "worker",
            "--url",
            "http://127.0.0.1:8081",
            "--name",
            "local",
            "--data-dir",
            "/srv/hopper/data",
            "--max-rss-gb",
            "24",
            "--workers",
            "8",
        ])
        .unwrap();

        let Command::Worker {
            url,
            name,
            data_dir,
            ..
        } = cli.command
        else {
            panic!("`worker` must parse as the worker command");
        };
        assert_eq!(url, "http://127.0.0.1:8081");
        assert_eq!(name.as_deref(), Some("local"));
        assert_eq!(data_dir, Some(PathBuf::from("/srv/hopper/data")));
        assert_eq!(cli.daemon.max_rss_gb, Some(24));
        assert_eq!(cli.daemon.workers.map(NonZeroUsize::get), Some(8));
    }

    #[test]
    fn the_analysis_flags_are_scans_own() {
        // Flattened rather than restated, so they cannot drift. Spot-check
        // that they reach us and land where the analysis reads them; the
        // point is that the declarations exist in one crate.
        let cli = Cli::try_parse_from([
            "postdoc",
            "--llm",
            "http://vllm:8000/v1",
            "-l",
            "50",
            "--zip-password",
            "infected",
            "serve",
        ])
        .unwrap();

        assert_eq!(cli.global.llm.as_deref(), Some("http://vllm:8000/v1"));
        assert_eq!(cli.global.level, Some(50));
        assert_eq!(cli.global.zip_passwords, vec!["infected".to_owned()]);
        assert!(
            cli.global.thresholds().is_none(),
            "no manual cutoff: the model's level grid decides"
        );
    }

    #[test]
    fn a_manual_cutoff_answers_hostile_versus_benign() {
        // `--threshold-hostile` alone collapses the suspicious cutoff onto it.
        // The level-space lookup a suspicious band needs is exactly what a
        // manual probability bypasses, so there is no band left to report.
        let cli = Cli::try_parse_from(["postdoc", "--threshold-hostile", "0.8", "serve"]).unwrap();
        let cutoffs = cli.global.thresholds().expect("a manual cutoff was named");
        assert_eq!(cutoffs.hostile, 0.8);
        assert_eq!(cutoffs.suspicious, 0.8);
    }

    #[test]
    fn a_manual_cutoff_and_a_level_cannot_both_be_given() {
        // Scan declares these mutually exclusive, because a manual
        // probability bypasses the level grid entirely. Flattening has to
        // carry the conflict across, not just the two flags.
        let both =
            Cli::try_parse_from(["postdoc", "--threshold-hostile", "0.8", "-l", "50", "serve"]);
        assert!(both.is_err(), "the conflict must survive being flattened");
    }

    #[test]
    fn serve_defaults_match_the_binary_it_replaces() {
        // A drop-in that listens elsewhere, or accepts a different body size,
        // is not a drop-in. These are `atomscan serve`'s defaults; if either
        // side moves, the swap stops being a binary rename.
        let cli = Cli::try_parse_from(["postdoc", "serve"]).unwrap();
        let Command::Serve {
            bind,
            max_size_mb,
            analysis_timeout,
            hopper,
            idle_worker_slots,
            ..
        } = cli.command
        else {
            panic!("`serve` must parse as the serve command");
        };

        assert_eq!(bind.to_string(), "127.0.0.1:49999");
        assert_eq!(max_size_mb, 100);
        assert!(
            cli.daemon.max_rss_gb.is_none(),
            "unset means postdoc resolves its own ceiling"
        );
        assert_eq!(
            analysis_timeout,
            scan::server::DEFAULT_ANALYSIS_TIMEOUT_SECS,
            "the timeout default is scan's own constant, not a copy of its value"
        );
        assert!(hopper.is_none(), "a pure analyze service by default");
        assert!(idle_worker_slots.is_none(), "half the request slots");
    }

    #[test]
    fn serve_accepts_an_operator_deployment() {
        let cli = Cli::try_parse_from([
            "postdoc",
            "serve",
            "--bind",
            "0.0.0.0:8080",
            "--hopper",
            "http://hopper:8081",
            "--token-file",
            "/etc/postdoc/token",
            "--allow-cidr",
            "10.0.0.0/8,192.168.0.0/16",
            "--allowed-dirs",
            "/srv/samples",
            "--max-rss-gb",
            "-1",
        ])
        .unwrap();

        let Command::Serve {
            bind,
            hopper,
            token_file,
            allow_cidr,
            allowed_dirs,
            ..
        } = cli.command
        else {
            panic!("`serve` must parse as the serve command");
        };
        assert_eq!(bind.to_string(), "0.0.0.0:8080");
        assert_eq!(hopper.as_deref(), Some("http://hopper:8081"));
        assert_eq!(token_file, Some(PathBuf::from("/etc/postdoc/token")));
        assert_eq!(allow_cidr.as_deref(), Some("10.0.0.0/8,192.168.0.0/16"));
        assert_eq!(allowed_dirs.as_deref(), Some("/srv/samples"));
        assert_eq!(cli.daemon.max_rss_gb, Some(-1));
    }

    #[test]
    fn throttling_can_be_turned_off_with_a_negative_ceiling() {
        // `-1` has to survive clap's hyphen handling, or a deployment that
        // relies on an external supervisor silently gets in-process
        // throttling instead.
        let cli = Cli::try_parse_from([
            "postdoc",
            "worker",
            "--url",
            "http://h",
            "--max-rss-gb",
            "-1",
        ])
        .unwrap();
        assert!(matches!(cli.command, Command::Worker { .. }));
        assert_eq!(cli.daemon.max_rss_gb, Some(-1));
    }
}
