//! Reading the source directory into a list of candidate entries.

use anyhow::{Context, Result};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::spec::{DirMode, ViewSpec};

/// One candidate entry from the source directory, with everything the
/// ordering and naming stages need already resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Absolute path to the real file.
    pub path: PathBuf,
    /// Path relative to the source root, `/`-separated.
    pub rel: String,
    /// Final path component, e.g. `IMG_2934.jpg`.
    pub name: String,
    /// Filename without its extension.
    pub stem: String,
    /// Extension without the dot; empty when there is none.
    pub ext: String,
    pub size: u64,
    /// Timestamps in nanoseconds, so entries written in the same second still
    /// order deterministically rather than by scan order.
    pub mtime: i128,
    pub ctime: i128,
    pub atime: i128,
    pub is_dir: bool,
}

impl Entry {
    fn from_path(path: PathBuf, rel: String, md: &fs::Metadata) -> Entry {
        let name = Path::new(&rel)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| rel.clone());
        // Split on the *last* dot, and never treat a leading dot as an
        // extension separator, so `.bashrc` keeps its whole name as the stem.
        let (stem, ext) = match name.rfind('.') {
            Some(i) if i > 0 => (name[..i].to_string(), name[i + 1..].to_string()),
            _ => (name.clone(), String::new()),
        };
        Entry {
            is_dir: md.is_dir(),
            size: md.size(),
            mtime: md.mtime() as i128 * 1_000_000_000 + md.mtime_nsec() as i128,
            ctime: md.ctime() as i128 * 1_000_000_000 + md.ctime_nsec() as i128,
            atime: md.atime() as i128 * 1_000_000_000 + md.atime_nsec() as i128,
            path,
            rel,
            name,
            stem,
            ext,
        }
    }
}

/// Collect every entry in `source` that the spec's traversal settings allow.
///
/// Filtering by glob happens later, in [`crate::order`]; this stage only
/// applies the structural rules (recursion, hidden files, directories).
pub fn scan(source: &Path, spec: &ViewSpec) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    if spec.recursive {
        scan_recursive(source, &mut out)?;
    } else {
        scan_flat(source, &mut out)?;
    }
    out.retain(|e| match spec.dirs {
        DirMode::Include => true,
        DirMode::Exclude => !e.is_dir,
        DirMode::Only => e.is_dir,
    });
    Ok(out)
}

fn scan_flat(source: &Path, out: &mut Vec<Entry>) -> Result<()> {
    let rd = fs::read_dir(source)
        .with_context(|| format!("cannot read directory {}", source.display()))?;
    for dirent in rd {
        let dirent = dirent?;
        let name = dirent.file_name().to_string_lossy().into_owned();
        if is_hidden(&name) {
            continue;
        }
        // A broken symlink has no metadata; skip it rather than aborting the
        // whole scan over one dangling link.
        let Ok(md) = dirent.metadata() else { continue };
        out.push(Entry::from_path(dirent.path(), name, &md));
    }
    Ok(())
}

fn scan_recursive(source: &Path, out: &mut Vec<Entry>) -> Result<()> {
    for dirent in walkdir::WalkDir::new(source)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        // Prune hidden directories entirely rather than walking into them.
        .filter_entry(|e| !is_hidden(&e.file_name().to_string_lossy()))
    {
        let Ok(dirent) = dirent else { continue };
        let Ok(md) = dirent.metadata() else { continue };
        let rel = dirent
            .path()
            .strip_prefix(source)
            .unwrap_or(dirent.path())
            .to_string_lossy()
            .into_owned();
        out.push(Entry::from_path(dirent.path().to_path_buf(), rel, &md));
    }
    Ok(())
}

/// Dotfiles are skipped: `*` doesn't match them in any shell, so surfacing
/// them in the view would only ever be noise — and it keeps our own
/// `.magicfs.json` state file out of the listing.
fn is_hidden(name: &str) -> bool {
    name.starts_with('.')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn splits_stem_and_extension() {
        let td = TempDir::new("entry-ext");
        td.touch("IMG_2934.jpg");
        td.touch("archive.tar.gz");
        td.touch("README");
        td.touch(".bashrc");

        let entries = scan(td.path(), &ViewSpec::default()).unwrap();
        let find = |n: &str| entries.iter().find(|e| e.name == n).cloned();

        let jpg = find("IMG_2934.jpg").unwrap();
        assert_eq!((jpg.stem.as_str(), jpg.ext.as_str()), ("IMG_2934", "jpg"));

        // Only the last dot counts, so `*.gz` matches but `*.tar.gz` naming
        // survives in the stem.
        let tgz = find("archive.tar.gz").unwrap();
        assert_eq!((tgz.stem.as_str(), tgz.ext.as_str()), ("archive.tar", "gz"));

        let readme = find("README").unwrap();
        assert_eq!((readme.stem.as_str(), readme.ext.as_str()), ("README", ""));

        // Dotfiles never reach the view at all.
        assert!(find(".bashrc").is_none());
    }

    #[test]
    fn dir_mode_controls_directory_visibility() {
        let td = TempDir::new("entry-dirs");
        td.touch("a.jpg");
        td.mkdir("sub");

        let names = |spec: &ViewSpec| {
            let mut n: Vec<_> = scan(td.path(), spec).unwrap().into_iter().map(|e| e.name).collect();
            n.sort();
            n
        };

        assert_eq!(names(&ViewSpec::default()), vec!["a.jpg", "sub"]);
        assert_eq!(
            names(&ViewSpec { dirs: DirMode::Exclude, ..Default::default() }),
            vec!["a.jpg"]
        );
        assert_eq!(
            names(&ViewSpec { dirs: DirMode::Only, ..Default::default() }),
            vec!["sub"]
        );
    }

    #[test]
    fn recursive_flattens_the_tree_and_skips_hidden_dirs() {
        let td = TempDir::new("entry-rec");
        td.touch("top.jpg");
        td.mkdir("2024");
        td.touch("2024/inner.jpg");
        td.mkdir(".git");
        td.touch(".git/config");

        let spec = ViewSpec { recursive: true, dirs: DirMode::Exclude, ..Default::default() };
        let mut rels: Vec<_> = scan(td.path(), &spec).unwrap().into_iter().map(|e| e.rel).collect();
        rels.sort();
        assert_eq!(rels, vec!["2024/inner.jpg", "top.jpg"]);
    }
}
