//! Small filesystem fixture used by the unit tests.

#![cfg(test)]

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);
static ENV: std::sync::Once = std::sync::Once::new();

/// Redirect the view registry and seen-lists into scratch files so tests never touch (or
/// race on) the real one under `~/.local/state`.
fn isolate_environment() {
    ENV.call_once(|| {
        let reg = std::env::temp_dir().join(format!("magicfs-test-registry-{}.json", std::process::id()));
        let base = std::env::temp_dir().join(format!("magicfs-test-base-{}", std::process::id()));
        let seen = std::env::temp_dir().join(format!("magicfs-test-seen-{}", std::process::id()));
        // Safe: runs once, before any test body observes the environment, and
        // always writes the same values.
        unsafe {
            std::env::set_var("MAGICFS_REGISTRY", &reg);
            std::env::set_var("MAGICFS_DIR", &base);
            std::env::set_var("MAGICFS_SEEN_DIR", &seen);
        }
    });
}

/// A scratch directory that deletes itself when the test ends.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(label: &str) -> TempDir {
        isolate_environment();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "magicfs-test-{label}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn mkdir(&self, rel: &str) {
        std::fs::create_dir_all(self.0.join(rel)).expect("mkdir");
    }

    pub fn touch(&self, rel: &str) {
        let p = self.0.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("mkdir -p");
        }
        std::fs::write(&p, b"").expect("touch");
    }

    pub fn write(&self, rel: &str, contents: &[u8]) {
        let p = self.0.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("mkdir -p");
        }
        std::fs::write(&p, contents).expect("write");
    }

    /// Create a file with an explicit mtime/atime, so ordering tests don't
    /// have to sleep to produce distinguishable timestamps.
    pub fn touch_at(&self, rel: &str, secs: i64) {
        self.touch(rel);
        self.set_mtime(rel, secs);
    }

    pub fn set_mtime(&self, rel: &str, secs: i64) {
        let p = self.0.join(rel);
        let c = CString::new(p.as_os_str().as_bytes()).expect("path has no NUL");
        let tv = libc::timeval { tv_sec: secs, tv_usec: 0 };
        let times = [tv, tv];
        let rc = unsafe { libc::utimes(c.as_ptr(), times.as_ptr()) };
        assert_eq!(rc, 0, "utimes failed for {}", p.display());
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
