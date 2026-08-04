//! The default backend: a real directory full of symlinks.
//!
//! Cheap enough that rebuilding the whole view on every reconfiguration is
//! instant, and it has no moving parts — no daemon to leak, no mount to go
//! stale, and it works on top of anything (NFS, sshfs, overlayfs).

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::naming::Named;
use crate::view::{STATE_FILE, View};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub created: usize,
    pub removed: usize,
    pub unchanged: usize,
}

impl Stats {
    pub fn total(&self) -> usize {
        self.created + self.unchanged
    }
}

/// Bring the view directory in line with `plan`.
///
/// Existing links that already point where they should are left alone, so a
/// reconfiguration only churns the entries that actually moved. That keeps
/// inotify-driven tools (and open file handles) from seeing spurious events.
pub fn apply(view: &View, plan: &[Named]) -> Result<Stats> {
    std::fs::create_dir_all(&view.root)
        .with_context(|| format!("creating view directory {}", view.root.display()))?;

    let existing = read_existing(&view.root)?;
    let desired: HashMap<&str, &Path> = plan
        .iter()
        .map(|n| (n.name.as_str(), n.entry.path.as_path()))
        .collect();

    let mut stats = Stats::default();

    for (name, target) in &existing {
        match desired.get(name.as_str()) {
            Some(want) if *want == target.as_path() => {}
            _ => {
                std::fs::remove_file(view.root.join(name))
                    .with_context(|| format!("removing stale link {name}"))?;
                stats.removed += 1;
            }
        }
    }

    for named in plan {
        let link = view.root.join(&named.name);
        if let Some(current) = existing.get(&named.name)
            && current == &named.entry.path
        {
            stats.unchanged += 1;
            continue;
        }
        std::os::unix::fs::symlink(&named.entry.path, &link)
            .with_context(|| format!("linking {} -> {}", link.display(), named.entry.path.display()))?;
        stats.created += 1;
    }

    Ok(stats)
}

/// Map of the symlinks currently in the view directory.
///
/// Anything that is *not* a symlink (and not our own state file) means we are
/// pointed at a directory holding real user data, so we refuse rather than
/// risk deleting it.
fn read_existing(root: &Path) -> Result<HashMap<String, PathBuf>> {
    let mut out = HashMap::new();
    let rd = match std::fs::read_dir(root) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("reading {}", root.display())),
    };

    let mut foreign = Vec::new();
    for dirent in rd {
        let dirent = dirent?;
        let name = dirent.file_name().to_string_lossy().into_owned();
        if name == STATE_FILE || name == format!("{STATE_FILE}.tmp") {
            continue;
        }
        // symlink_metadata does not follow the link, so a link to a directory
        // is still recognised as a link.
        let md = dirent.path().symlink_metadata()?;
        if !md.file_type().is_symlink() {
            foreign.push(name);
            continue;
        }
        let target = std::fs::read_link(dirent.path())?;
        out.insert(name, target);
    }

    if !foreign.is_empty() {
        foreign.sort();
        let shown: Vec<_> = foreign.iter().take(5).cloned().collect();
        bail!(
            "{} contains {} real file(s) that magicfs did not create ({}{}) — \
             refusing to touch it.\nPick a different location with --out, or \
             move those files elsewhere.",
            root.display(),
            foreign.len(),
            shown.join(", "),
            if foreign.len() > shown.len() { ", ..." } else { "" }
        );
    }
    Ok(out)
}

/// Tear the view directory down: every symlink, the state file, then the
/// directory itself. Real files are never removed — only links to them.
pub fn close(view: &View) -> Result<()> {
    let existing = read_existing(&view.root)?;
    for name in existing.keys() {
        let _ = std::fs::remove_file(view.root.join(name));
    }
    let _ = std::fs::remove_file(view.root.join(STATE_FILE));
    std::fs::remove_dir(&view.root)
        .with_context(|| format!("removing view directory {}", view.root.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::scan;
    use crate::naming::render;
    use crate::order::arrange;
    use crate::spec::{DirMode, SortKey, ViewSpec};
    use crate::testutil::TempDir;

    /// Build a view of `source` inside `root` and return the resulting stats.
    fn build(root: &Path, source: &Path, spec: &ViewSpec) -> Result<(View, Stats)> {
        let view = View {
            root: root.to_path_buf(),
            source: source.to_path_buf(),
            spec: spec.clone(),
        };
        let entries = arrange(scan(source, spec)?, spec)?;
        let plan = render(entries, spec);
        let stats = apply(&view, &plan)?;
        Ok((view, stats))
    }

    /// What a shell's `*` would expand to inside the view.
    fn glob(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(root)
            .unwrap()
            .flatten()
            .map(|d| d.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.'))
            .collect();
        names.sort(); // the shell sorts; this is the whole point
        names
    }

    #[test]
    fn links_resolve_to_the_real_files_in_the_requested_order() {
        let src = TempDir::new("sym-src");
        src.write("alpha.jpg", b"A");
        src.write("beta.jpg", b"B");
        src.set_mtime("alpha.jpg", 2_000);
        src.set_mtime("beta.jpg", 1_000);

        let out = TempDir::new("sym-out");
        let root = out.path().join("v");
        let spec = ViewSpec { sort: SortKey::Time, dirs: DirMode::Exclude, ..Default::default() };
        let (_, stats) = build(&root, src.path(), &spec).unwrap();

        assert_eq!(stats.created, 2);
        assert_eq!(glob(&root), vec!["001-alpha.jpg", "002-beta.jpg"]);

        // Reading through the link must reach the real file's contents.
        assert_eq!(std::fs::read(root.join("001-alpha.jpg")).unwrap(), b"A");
        assert_eq!(std::fs::read(root.join("002-beta.jpg")).unwrap(), b"B");
    }

    #[test]
    fn reconfiguring_only_churns_what_moved() {
        let src = TempDir::new("sym-churn");
        for i in 0..5 {
            src.touch_at(&format!("f{i}.jpg"), 1_000 + i as i64);
        }
        let out = TempDir::new("sym-churn-out");
        let root = out.path().join("v");

        let asc = ViewSpec { sort: SortKey::Time, reverse: true, dirs: DirMode::Exclude, ..Default::default() };
        let (_, first) = build(&root, src.path(), &asc).unwrap();
        assert_eq!((first.created, first.removed), (5, 0));

        // Rebuilding with an identical spec should be a complete no-op.
        let (_, again) = build(&root, src.path(), &asc).unwrap();
        assert_eq!((again.created, again.removed, again.unchanged), (0, 0, 5));

        // Reversing relabels every entry except the midpoint, which is the
        // 3rd of 5 in both directions and so keeps its link untouched.
        let desc = ViewSpec { reverse: false, ..asc };
        let (_, flipped) = build(&root, src.path(), &desc).unwrap();
        assert_eq!((flipped.created, flipped.removed, flipped.unchanged), (4, 4, 1));
        assert_eq!(
            glob(&root),
            vec!["001-f4.jpg", "002-f3.jpg", "003-f2.jpg", "004-f1.jpg", "005-f0.jpg"]
        );
    }

    #[test]
    fn stale_links_are_cleared_when_the_filter_narrows() {
        let src = TempDir::new("sym-stale");
        src.touch("a.jpg");
        src.touch("b.png");
        let out = TempDir::new("sym-stale-out");
        let root = out.path().join("v");

        build(&root, src.path(), &ViewSpec { dirs: DirMode::Exclude, ..Default::default() }).unwrap();
        assert_eq!(glob(&root).len(), 2);

        let narrowed = ViewSpec {
            filter: vec!["png".into()],
            dirs: DirMode::Exclude,
            ..Default::default()
        };
        build(&root, src.path(), &narrowed).unwrap();
        assert_eq!(glob(&root), vec!["001-b.png"]);
    }

    #[test]
    fn refuses_to_manage_a_directory_holding_real_files() {
        let src = TempDir::new("sym-guard-src");
        src.touch("a.jpg");
        let out = TempDir::new("sym-guard-out");
        let root = out.path().join("v");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("important.txt"), b"do not delete").unwrap();

        let err = build(&root, src.path(), &ViewSpec::default()).unwrap_err().to_string();
        assert!(err.contains("important.txt"), "got: {err}");
        assert!(err.contains("refusing"), "got: {err}");
        // And the file is still there.
        assert!(root.join("important.txt").exists());
    }

    #[test]
    fn close_removes_links_but_never_the_real_files() {
        let src = TempDir::new("sym-close-src");
        src.write("keep.jpg", b"precious");
        let out = TempDir::new("sym-close-out");
        let root = out.path().join("v");

        let (view, _) = build(&root, src.path(), &ViewSpec { dirs: DirMode::Exclude, ..Default::default() }).unwrap();
        view.save().unwrap();
        assert!(root.exists());

        close(&view).unwrap();
        assert!(!root.exists(), "view directory should be gone");
        assert_eq!(std::fs::read(src.path().join("keep.jpg")).unwrap(), b"precious");
    }

    #[test]
    fn directories_are_linked_and_remain_traversable() {
        let src = TempDir::new("sym-dir-src");
        src.mkdir("album");
        src.write("album/inner.jpg", b"I");
        let out = TempDir::new("sym-dir-out");
        let root = out.path().join("v");

        build(&root, src.path(), &ViewSpec::default()).unwrap();
        assert_eq!(glob(&root), vec!["001-album"]);
        // The link points at a directory you can still descend into.
        assert_eq!(std::fs::read(root.join("001-album/inner.jpg")).unwrap(), b"I");
    }
}
