//! Process-owned scratch on the existing temporary filesystem.
//!
//! Dependency TempDirs clean up on unwind, but not on SIGKILL, abort or
//! process::exit. An ownership lock lets the next startup reclaim an abandoned
//! tree without touching a live process's files. The server also owns its idle
//! child's tree, so worker OOM kills do not accumulate until a server restart.

use std::fs::File;
use std::io;
use std::path::Path;
use std::process::Command;

const PREFIX: &str = "postdoc-tmp-";
const OWNER: &str = ".postdoc-owner";

/// Scratch storage, removed while the ownership lock is still held.
#[derive(Debug)]
pub(crate) struct Scratch {
    dir: Option<tempfile::TempDir>,
    _owner: File,
}

impl Scratch {
    /// Create scratch beneath the current temporary directory.
    pub(crate) fn new() -> io::Result<Self> {
        Self::in_dir(&std::env::temp_dir())
    }

    fn in_dir(parent: &Path) -> io::Result<Self> {
        let dir = tempfile::Builder::new().prefix(PREFIX).tempdir_in(parent)?;
        // Lock before publishing the marker: a concurrent startup must never
        // mistake a not-yet-locked new directory for an abandoned one.
        let owner = tempfile::NamedTempFile::new_in(dir.path())?;
        owner.as_file().lock()?;
        let owner = owner.persist(dir.path().join(OWNER)).map_err(|e| e.error)?;
        Ok(Self {
            dir: Some(dir),
            _owner: owner,
        })
    }

    fn path(&self) -> &Path {
        // `dir` is taken only by Drop, after the last use of this object.
        self.dir.as_ref().map_or(Path::new(""), |dir| dir.path())
    }

    /// Make the child's dependencies use this scratch directory too.
    pub(crate) fn configure(&self, cmd: &mut Command) {
        for key in ["TMPDIR", "TMP", "TEMP"] {
            cmd.env(key, self.path());
        }
    }

    /// Establish this process's scratch before dependencies read the environment.
    ///
    /// # Safety
    /// Call before spawning threads or reading temporary-directory settings.
    #[allow(unsafe_code)]
    pub(crate) unsafe fn install() -> io::Result<Self> {
        let parent = std::env::temp_dir();
        let scratch = Self::in_dir(&parent)?;
        for key in ["TMPDIR", "TMP", "TEMP"] {
            // SAFETY: the caller guarantees single-threaded startup.
            unsafe { std::env::set_var(key, scratch.path()) };
        }
        Ok(scratch)
    }

    /// Reclaim abandoned siblings after logging is ready. A cleanup failure
    /// must not prevent an otherwise healthy process from starting.
    pub(crate) fn cleanup_abandoned(&self) {
        if let Some(parent) = self.path().parent()
            && let Err(error) = cleanup_stale(parent)
        {
            tracing::warn!(path = %parent.display(), %error, "cannot sweep abandoned temporary storage");
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Some(dir) = self.dir.take() {
            let path = dir.path().to_path_buf();
            if let Err(error) = dir.close() {
                tracing::warn!(path = %path.display(), %error, "temporary storage cleanup failed; next startup will retry");
            }
        }
    }
}

/// Only marked, unlocked directories belonging to this user are eligible.
fn cleanup_stale(parent: &Path) -> io::Result<usize> {
    let mut removed = 0;
    for entry in std::fs::read_dir(parent)? {
        let Ok(entry) = entry else { continue };
        if !entry.file_name().to_string_lossy().starts_with(PREFIX)
            || !entry.file_type().is_ok_and(|kind| kind.is_dir())
        {
            continue;
        }
        let path = entry.path();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // SAFETY: geteuid has no pointer arguments or side effects.
            #[allow(unsafe_code)]
            let uid = unsafe { libc::geteuid() };
            if !entry.metadata().is_ok_and(|meta| meta.uid() == uid) {
                continue;
            }
        }
        let marker = path.join(OWNER);
        if !std::fs::symlink_metadata(&marker).is_ok_and(|meta| meta.is_file()) {
            continue;
        }
        let mut options = File::options();
        options.read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let Ok(owner) = options.open(&marker) else {
            continue;
        };
        match owner.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => continue,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "cannot check temporary storage ownership");
                continue;
            }
        }
        // Keep the lock through removal, including removal of the marker.
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                removed += 1;
                tracing::info!(path = %path.display(), "removed abandoned postdoc temporary storage");
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "cannot remove abandoned temporary storage");
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_preserves_live_and_unowned_files() {
        let parent = tempfile::tempdir().unwrap();
        let live = Scratch::in_dir(parent.path()).unwrap();
        std::fs::write(live.path().join("live"), b"in use").unwrap();
        let unowned = parent.path().join("postdoc-tmp-unmarked");
        std::fs::create_dir(&unowned).unwrap();
        std::fs::write(unowned.join("keep"), b"unrelated").unwrap();
        assert_eq!(cleanup_stale(parent.path()).unwrap(), 0);
        assert!(live.path().join("live").exists());
        assert!(unowned.join("keep").exists());
        let path = live.path().to_path_buf();
        drop(live);
        assert!(!path.exists());
    }

    #[test]
    fn startup_reclaims_scratch_after_process_death() {
        let parent = tempfile::tempdir().unwrap();
        let ready = parent.path().join("ready");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "temp_space::tests::crash_fixture", "--ignored"])
            .env("POSTDOC_TMP_TEST_PARENT", parent.path())
            .spawn()
            .unwrap();
        let start = std::time::Instant::now();
        while !ready.exists() {
            assert!(child.try_wait().unwrap().is_none(), "fixture exited early");
            assert!(start.elapsed() < std::time::Duration::from_secs(10));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // The tree exists but its live owner's lock prevents startup cleanup.
        assert_eq!(cleanup_stale(parent.path()).unwrap(), 0);
        child.kill().unwrap();
        child.wait().unwrap();
        let abandoned = std::fs::read_to_string(&ready).unwrap();
        assert!(Path::new(&abandoned).join("dependency/member").exists());
        assert_eq!(cleanup_stale(parent.path()).unwrap(), 1);
        assert!(!Path::new(&abandoned).exists());
    }

    #[test]
    #[ignore = "subprocess fixture for startup cleanup"]
    fn crash_fixture() {
        let parent = std::env::var_os("POSTDOC_TMP_TEST_PARENT").unwrap();
        let parent = Path::new(&parent);
        let scratch = Scratch::in_dir(parent).unwrap();
        std::fs::create_dir(scratch.path().join("dependency")).unwrap();
        std::fs::write(scratch.path().join("dependency/member"), b"scratch").unwrap();
        std::fs::write(parent.join("ready"), scratch.path().to_str().unwrap()).unwrap();
        loop {
            std::thread::park();
        }
    }

    #[test]
    #[cfg(unix)]
    fn cleanup_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;
        let parent = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        std::fs::write(external.path().join(OWNER), b"keep").unwrap();
        symlink(external.path(), parent.path().join("postdoc-tmp-link")).unwrap();
        let marked = parent.path().join("postdoc-tmp-marker-link");
        std::fs::create_dir(&marked).unwrap();
        symlink(external.path().join(OWNER), marked.join(OWNER)).unwrap();
        assert_eq!(cleanup_stale(parent.path()).unwrap(), 0);
        assert!(external.path().join(OWNER).exists());
        assert!(marked.exists());
    }
}
