//! Where views live on disk, and how a running `magicfs` command finds the one
//! the user is standing in.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::spec::ViewSpec;

/// Marker file at the root of every view. Dotted so it stays out of `*`.
pub const STATE_FILE: &str = ".magicfs.json";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct View {
    /// The directory the user cds into.
    pub root: PathBuf,
    /// The real directory being presented.
    pub source: PathBuf,
    pub spec: ViewSpec,
}

impl View {
    pub fn state_path(&self) -> PathBuf {
        self.root.join(STATE_FILE)
    }

    /// Persist the view's configuration into its own root directory.
    ///
    /// Written atomically via rename so a command interrupted mid-write can't
    /// leave a view with a truncated, unparseable state file.
    pub fn save(&self) -> Result<()> {
        let path = self.state_path();
        let tmp = self.root.join(format!("{STATE_FILE}.tmp"));
        let json = serde_json::to_vec_pretty(self)?;
        std::fs::write(&tmp, &json)
            .with_context(|| format!("writing view state to {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("installing view state at {}", path.display()))?;
        // The registry is only an index for `magicfs list`; the view itself is
        // fully described by its own state file, so a failure here must not
        // fail the operation the user actually asked for.
        let _ = registry_add(&self.root);
        Ok(())
    }

    pub fn load(root: &Path) -> Result<View> {
        let path = root.join(STATE_FILE);
        let bytes = std::fs::read(&path)
            .with_context(|| format!("reading view state from {}", path.display()))?;
        let mut view: View = serde_json::from_slice(&bytes)
            .with_context(|| format!("{} is not a valid magicfs view state", path.display()))?;
        // Trust the location we actually found it at over the recorded path,
        // so a view still works after its parent directory is moved.
        view.root = root.to_path_buf();
        Ok(view)
    }
}

/// Root directory that generated views are placed under.
///
/// Prefers the per-user runtime dir (tmpfs, cleaned on logout) so views never
/// outlive a session or pollute the directories being viewed — a photo
/// directory that gets backed up or synced should not gain a sibling full of
/// symlinks.
pub fn base_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MAGICFS_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("magicfs");
    }
    std::env::temp_dir().join(format!("magicfs-{}", unsafe { libc::getuid() }))
}

fn registry_path() -> PathBuf {
    if let Some(p) = std::env::var_os("MAGICFS_REGISTRY") {
        return PathBuf::from(p);
    }
    state_home().join("magicfs/registry.json")
}

/// `$XDG_STATE_HOME`, falling back to `~/.local/state` as the spec says.
pub fn state_home() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(std::env::temp_dir)
}

fn registry_read() -> Vec<PathBuf> {
    std::fs::read(registry_path())
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<PathBuf>>(&b).ok())
        .unwrap_or_default()
}

fn registry_write(roots: &[PathBuf]) -> Result<()> {
    let path = registry_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(roots)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn registry_add(root: &Path) -> Result<()> {
    let mut roots = registry_read();
    if !roots.iter().any(|r| r == root) {
        roots.push(root.to_path_buf());
        registry_write(&roots)?;
    }
    Ok(())
}

pub fn registry_remove(root: &Path) -> Result<()> {
    let mut roots = registry_read();
    let before = roots.len();
    roots.retain(|r| r != root);
    if roots.len() != before {
        registry_write(&roots)?;
    }
    Ok(())
}

/// Every view we know about, dropping registry entries whose directory is
/// gone so a crashed or manually-deleted view self-heals out of the list.
pub fn list_views() -> Vec<View> {
    let mut roots = registry_read();

    // Also sweep the default base dir, so views survive a lost registry file.
    if let Ok(rd) = std::fs::read_dir(base_dir()) {
        for dirent in rd.flatten() {
            let p = dirent.path();
            if p.join(STATE_FILE).exists() && !roots.contains(&p) {
                roots.push(p);
            }
        }
    }

    let mut views = Vec::new();
    let mut live_roots = Vec::new();
    for root in roots {
        if let Ok(view) = View::load(&root) {
            live_roots.push(root);
            views.push(view);
        }
    }
    let _ = registry_write(&live_roots);
    views.sort_by(|a, b| a.root.cmp(&b.root));
    views
}

/// Find the view containing `start`, by walking up to the filesystem root.
///
/// Walking up (rather than requiring the exact root) means `magicfs shuffle`
/// works from any subdirectory of a recursive view.
pub fn find_containing(start: &Path) -> Option<View> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(STATE_FILE).exists()
            && let Ok(view) = View::load(dir)
        {
            return Some(view);
        }
        cur = dir.parent();
    }
    None
}

/// The view for the current working directory, if we're inside one.
pub fn current() -> Result<Option<View>> {
    let cwd = std::env::current_dir().context("cannot determine current directory")?;
    Ok(find_containing(&cwd))
}

/// Characters used in a view's id: lowercase alphanumerics minus the pairs
/// that are hard to tell apart when read off a prompt (`0`/`o`, `1`/`l`/`i`).
const ID_ALPHABET: &[u8] = b"23456789abcdefghjkmnpqrstuvwxyz";
const ID_LEN: usize = 3;

/// Choose the directory for a new view over `source`.
///
/// Every call gets its own directory — `~/photos` opened twice yields
/// `photos-4dk` and `photos-q7f`, never the same one. Reusing a view would
/// mean a second terminal silently reordering the directory the first one is
/// standing in, and two different directories that happen to share a basename
/// would fight over the same name.
///
/// The name is claimed by creating it, which is atomic, so two magicfs
/// processes racing on the same id cannot both win.
pub fn allocate_root(source: &Path, base: &Path, explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(root) = explicit {
        std::fs::create_dir_all(&root)
            .with_context(|| format!("creating {}", root.display()))?;
        return Ok(root);
    }
    let stem = source
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".to_string());

    let mut entropy = crate::spec::fresh_seed();
    for attempt in 0.. {
        entropy = crate::spec::mix64(entropy ^ attempt);
        let candidate = base.join(format!("{stem}-{}", id_from(entropy)));
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            // Taken — by another view, or by another magicfs a moment ago.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e).with_context(|| format!("creating {}", candidate.display()));
            }
        }
    }
    unreachable!()
}

fn id_from(mut n: u64) -> String {
    let mut out = String::with_capacity(ID_LEN);
    for _ in 0..ID_LEN {
        out.push(ID_ALPHABET[(n % ID_ALPHABET.len() as u64) as usize] as char);
        n /= ID_ALPHABET.len() as u64;
    }
    out
}

/// Every directory under `base` that is not a live view — the leftovers of
/// views closed while a shell was standing in them, plus anything a crash left
/// behind. Used by `magicfs clean`.
pub fn stale_dirs(base: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(base) else {
        return Vec::new();
    };
    rd.flatten()
        .map(|d| d.path())
        .filter(|p| p.is_dir() && !p.join(STATE_FILE).exists())
        .collect()
}

/// Reject sources that would make a view of a view.
pub fn validate_source(source: &Path) -> Result<()> {
    if !source.is_dir() {
        bail!("{} is not a directory", source.display());
    }
    if let Some(existing) = find_containing(source) {
        bail!(
            "{} is inside the magicfs view at {} — point magicfs at the real \
             directory ({}) instead",
            source.display(),
            existing.root.display(),
            existing.source.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn view_at(root: &Path, source: &Path) -> View {
        View {
            root: root.to_path_buf(),
            source: source.to_path_buf(),
            spec: ViewSpec::default(),
        }
    }

    #[test]
    fn state_round_trips_through_disk() {
        let td = TempDir::new("view-state");
        td.mkdir("v");
        let root = td.path().join("v");
        let mut view = view_at(&root, Path::new("/some/source"));
        view.spec.limit = Some(7);
        view.spec.filter = vec!["png".into()];
        view.save().unwrap();

        let loaded = View::load(&root).unwrap();
        assert_eq!(loaded.source, PathBuf::from("/some/source"));
        assert_eq!(loaded.spec.limit, Some(7));
        assert_eq!(loaded.spec.filter, vec!["png".to_string()]);
    }

    #[test]
    fn discovery_walks_up_from_a_subdirectory() {
        let td = TempDir::new("view-find");
        td.mkdir("v/deep/deeper");
        let root = td.path().join("v");
        view_at(&root, Path::new("/src")).save().unwrap();

        let found = find_containing(&root.join("deep/deeper")).expect("should find view");
        assert_eq!(found.root, root);
        assert!(find_containing(td.path()).is_none());
    }

    #[test]
    fn allocate_never_hands_out_the_same_directory_twice() {
        // Reuse would mean a second terminal reordering the view the first one
        // is standing in.
        let td = TempDir::new("view-alloc-unique");
        let base = td.path().join("base");
        std::fs::create_dir_all(&base).unwrap();
        let source = PathBuf::from("/home/u/photos");

        let mut seen = std::collections::HashSet::new();
        for _ in 0..50 {
            let root = allocate_root(&source, &base, None).unwrap();
            assert!(root.is_dir(), "the name must be claimed, not just chosen");
            assert!(seen.insert(root.clone()), "handed out {root:?} twice");
        }
    }

    #[test]
    fn allocate_keeps_the_source_name_visible_and_readable() {
        let td = TempDir::new("view-alloc-name");
        let base = td.path().join("base");
        std::fs::create_dir_all(&base).unwrap();

        let root = allocate_root(Path::new("/home/u/photos"), &base, None).unwrap();
        let name = root.file_name().unwrap().to_string_lossy().into_owned();
        let id = name.strip_prefix("photos-").expect("should read `photos-<id>`");
        assert_eq!(id.len(), ID_LEN, "got {name}");
        assert!(
            id.bytes().all(|b| ID_ALPHABET.contains(&b)),
            "id should avoid look-alike characters: {name}"
        );
    }

    #[test]
    fn two_directories_with_the_same_name_get_separate_views() {
        let td = TempDir::new("view-alloc-clash");
        let base = td.path().join("base");
        std::fs::create_dir_all(&base).unwrap();

        let a = allocate_root(Path::new("/home/u/photos"), &base, None).unwrap();
        let b = allocate_root(Path::new("/mnt/backup/photos"), &base, None).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn stale_dirs_finds_leftovers_but_not_live_views() {
        let td = TempDir::new("view-stale");
        let base = td.path().join("base");
        std::fs::create_dir_all(&base).unwrap();

        let live = allocate_root(Path::new("/src"), &base, None).unwrap();
        view_at(&live, Path::new("/src")).save().unwrap();
        let husk = allocate_root(Path::new("/src"), &base, None).unwrap();

        assert_eq!(stale_dirs(&base), vec![husk]);
    }

    #[test]
    fn refuses_to_build_a_view_of_a_view() {
        let td = TempDir::new("view-nested");
        td.mkdir("v");
        let root = td.path().join("v");
        view_at(&root, Path::new("/src")).save().unwrap();

        let err = validate_source(&root).unwrap_err().to_string();
        assert!(err.contains("inside the magicfs view"), "got: {err}");
        assert!(err.contains("/src"), "error should point at the real source");
    }

    #[test]
    fn load_prefers_the_actual_location_over_the_recorded_one() {
        // A view directory that got moved should still resolve to where it is.
        let td = TempDir::new("view-moved");
        td.mkdir("original");
        let original = td.path().join("original");
        view_at(&original, Path::new("/src")).save().unwrap();

        let moved = td.path().join("moved");
        std::fs::rename(&original, &moved).unwrap();

        let loaded = View::load(&moved).unwrap();
        assert_eq!(loaded.root, moved);
    }
}
