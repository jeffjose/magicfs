//! Remembering which files have already been handed to a command, so
//! `--unseen` can show only the ones that arrived since.
//!
//! The use case is a directory that fills up while you watch it — a render
//! finishing one video at a time — and a review loop of
//! `magicfs -s time --unseen smplayer *` that should only ever show you the
//! new ones.
//!
//! "Seen" means *handed to a command by magicfs*. atime would need no state,
//! but it lies in both directions: `noatime` mounts never update it, and
//! thumbnailers, backups and the player's own probing update it for files
//! nobody looked at.
//!
//! A record is keyed on the file's relative path *and* its mtime and size, so a
//! file that changes after being seen counts as new again. That covers both a
//! re-render under the same name and a file that was still being written when
//! it was handed over — once it finishes, it comes back.
//!
//! The store lives under `$XDG_STATE_HOME`, never in the source directory:
//! magicfs does not write to the directories it presents.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::entry::Entry;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
struct Record {
    rel: String,
    mtime: i128,
    size: u64,
    /// Which marking this came from, so `unsee --last` can undo one.
    batch: u64,
    /// When it was marked, in seconds since the epoch.
    at: u64,
}

#[derive(Serialize, Deserialize, Debug, Default)]
struct Store {
    /// The directory this store describes — for a human reading the file, since
    /// the file name only carries a hash of it.
    source: PathBuf,
    next_batch: u64,
    records: Vec<Record>,
}

/// The seen-set for one source directory.
pub struct Seen {
    path: PathBuf,
    store: Store,
}

impl Seen {
    /// Load the seen-set for `source`. A directory never marked has an empty one.
    pub fn load(source: &Path) -> Result<Seen> {
        let source = source.canonicalize().unwrap_or_else(|_| source.to_path_buf());
        let path = store_path(&source);
        let store = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("{} is not a valid seen-list", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Store { source, ..Default::default() }
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Seen { path, store })
    }

    /// Whether this exact version of the file has been handed over before.
    pub fn contains(&self, entry: &Entry) -> bool {
        self.store
            .records
            .iter()
            .any(|r| r.rel == entry.rel && r.mtime == entry.mtime && r.size == entry.size)
    }

    /// Record these entries as seen, as one batch. Returns how many were new.
    pub fn mark<'a>(&mut self, entries: impl IntoIterator<Item = &'a Entry>) -> usize {
        let batch = self.store.next_batch;
        let at = now();
        let mut added = 0;
        for entry in entries {
            if self.contains(entry) {
                continue;
            }
            // One record per path: a newer version replaces the old one.
            self.store.records.retain(|r| r.rel != entry.rel);
            self.store.records.push(Record {
                rel: entry.rel.clone(),
                mtime: entry.mtime,
                size: entry.size,
                batch,
                at,
            });
            added += 1;
        }
        if added > 0 {
            self.store.next_batch += 1;
        }
        added
    }

    /// Forget the most recent batch. Returns how many files it held.
    pub fn unmark_last(&mut self) -> usize {
        let Some(last) = self.store.records.iter().map(|r| r.batch).max() else {
            return 0;
        };
        self.forget(|r| r.batch == last)
    }

    /// Forget these entries, whatever version of them was seen.
    pub fn unmark<'a>(&mut self, entries: impl IntoIterator<Item = &'a Entry>) -> usize {
        let rels: std::collections::HashSet<&str> =
            entries.into_iter().map(|e| e.rel.as_str()).collect();
        self.forget(|r| rels.contains(r.rel.as_str()))
    }

    /// Forget everything.
    pub fn clear(&mut self) -> usize {
        self.forget(|_| true)
    }

    fn forget(&mut self, doomed: impl Fn(&Record) -> bool) -> usize {
        let before = self.store.records.len();
        self.store.records.retain(|r| !doomed(r));
        before - self.store.records.len()
    }

    /// How many of `entries` have been seen.
    pub fn count_in(&self, entries: &[Entry]) -> usize {
        entries.iter().filter(|e| self.contains(e)).count()
    }

    /// Seconds since the most recent marking, if there has been one.
    pub fn last_marked_ago(&self) -> Option<u64> {
        let at = self.store.records.iter().map(|r| r.at).max()?;
        Some(now().saturating_sub(at))
    }

    /// Size of the most recent batch.
    pub fn last_batch_len(&self) -> usize {
        let Some(last) = self.store.records.iter().map(|r| r.batch).max() else {
            return 0;
        };
        self.store.records.iter().filter(|r| r.batch == last).count()
    }

    /// Write the store back, dropping records for files that no longer exist
    /// so it doesn't grow forever in a directory that gets cleaned out.
    ///
    /// Atomic via rename, like the view state: an interrupted write must not
    /// leave a truncated file that makes every later `--unseen` fail.
    pub fn save(&mut self) -> Result<()> {
        let source = self.store.source.clone();
        self.store.records.retain(|r| source.join(&r.rel).symlink_metadata().is_ok());

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&self.store)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("installing {}", self.path.display()))?;
        Ok(())
    }
}

/// Where seen-lists are kept: `$XDG_STATE_HOME/magicfs/seen`.
///
/// State, not cache — `~/.cache` is fair game for any cleanup tool, and losing
/// this means every old file shows up as new again.
pub fn dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MAGICFS_SEEN_DIR") {
        return PathBuf::from(dir);
    }
    crate::view::state_home().join("magicfs/seen")
}

/// One file per source directory, named so a person can tell them apart:
/// `renders-3f9a0c1e2b4d5a6f.json`. The hash keeps two directories that share a
/// basename apart.
fn store_path(source: &Path) -> PathBuf {
    let stem = source
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".to_string());
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a
    for byte in source.as_os_str().as_encoded_bytes() {
        h ^= *byte as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    dir().join(format!("{stem}-{h:016x}.json"))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `90` → `1m ago`, for the one line that says when you last looked.
pub fn ago(secs: u64) -> String {
    match secs {
        0..60 => "just now".to_string(),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::scan;
    use crate::spec::ViewSpec;
    use crate::testutil::TempDir;

    fn entries(td: &TempDir) -> Vec<Entry> {
        let mut e = scan(td.path(), &ViewSpec::default()).unwrap();
        e.sort_by(|a, b| a.rel.cmp(&b.rel));
        e
    }

    #[test]
    fn marked_files_are_remembered_across_loads() {
        let td = TempDir::new("seen-roundtrip");
        td.touch("a.mp4");
        td.touch("b.mp4");
        let all = entries(&td);

        let mut seen = Seen::load(td.path()).unwrap();
        assert_eq!(seen.mark(&all[..1]), 1);
        seen.save().unwrap();

        let seen = Seen::load(td.path()).unwrap();
        assert!(seen.contains(&all[0]));
        assert!(!seen.contains(&all[1]));
    }

    #[test]
    fn a_file_that_changes_after_being_seen_is_new_again() {
        // A re-render, or a file that was still being written when handed over.
        let td = TempDir::new("seen-changed");
        td.touch_at("a.mp4", 1_000);
        let mut seen = Seen::load(td.path()).unwrap();
        seen.mark(&entries(&td));

        td.write("a.mp4", b"finished");
        td.set_mtime("a.mp4", 2_000);
        assert!(!seen.contains(&entries(&td)[0]));
    }

    #[test]
    fn unmark_last_undoes_only_the_latest_batch() {
        let td = TempDir::new("seen-undo");
        td.touch("a.mp4");
        td.touch("b.mp4");
        td.touch("c.mp4");
        let all = entries(&td);

        let mut seen = Seen::load(td.path()).unwrap();
        seen.mark(&all[..1]);
        seen.mark(&all[1..]);
        assert_eq!(seen.last_batch_len(), 2);
        assert_eq!(seen.unmark_last(), 2);
        assert!(seen.contains(&all[0]));
        assert!(!seen.contains(&all[1]) && !seen.contains(&all[2]));
    }

    #[test]
    fn remarking_seen_files_does_not_start_an_empty_batch() {
        // Otherwise `unsee --last` would undo nothing after a no-op marking.
        let td = TempDir::new("seen-noop");
        td.touch("a.mp4");
        let all = entries(&td);
        let mut seen = Seen::load(td.path()).unwrap();
        seen.mark(&all);
        assert_eq!(seen.mark(&all), 0);
        assert_eq!(seen.unmark_last(), 1);
    }

    #[test]
    fn saving_prunes_files_that_are_gone() {
        let td = TempDir::new("seen-prune");
        td.touch("a.mp4");
        td.touch("b.mp4");
        let mut seen = Seen::load(td.path()).unwrap();
        seen.mark(&entries(&td));
        std::fs::remove_file(td.path().join("a.mp4")).unwrap();
        seen.save().unwrap();
        assert_eq!(seen.store.records.len(), 1);
    }

    #[test]
    fn directories_sharing_a_name_keep_separate_lists() {
        let td = TempDir::new("seen-clash");
        td.mkdir("x/renders");
        td.mkdir("y/renders");
        assert_ne!(
            store_path(&td.path().join("x/renders")),
            store_path(&td.path().join("y/renders"))
        );
    }
}
