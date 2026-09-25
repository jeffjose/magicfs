//! The description of *how* a source directory should be presented.
//!
//! A `ViewSpec` is the entire user-facing configuration of a view: which
//! entries survive, what order they land in, and what they get called. It is
//! serialised into the view's state file so that `magicfs sort time` can pick
//! up where the last command left off.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// The default name template: `0001-IMG_2934.jpg`.
pub const DEFAULT_NAME_FORMAT: &str = "{i}-{name}";

/// Index prefixes are padded to at least this width, so a directory that grows
/// past 9 or 99 entries doesn't suddenly re-order itself under a plain sort.
pub const MIN_PAD: usize = 3;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SortKey {
    /// Byte-wise filename order — what the shell would have done anyway.
    Name,
    /// Filename order, but runs of digits compare numerically (`img2` < `img10`).
    Natural,
    /// Modification time.
    Time,
    /// Inode change time.
    Ctime,
    /// Access time.
    Atime,
    /// Creation time, falling back to mtime where there is none.
    Created,
    Size,
    /// Group by extension, then by name within each extension.
    Ext,
    /// Deterministic shuffle, driven by [`ViewSpec::seed`].
    Random,
}

impl SortKey {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "name" | "n" | "alpha" => SortKey::Name,
            "natural" | "nat" | "version" | "v" => SortKey::Natural,
            "time" | "mtime" | "modified" | "date" | "t" => SortKey::Time,
            "ctime" | "changed" => SortKey::Ctime,
            "atime" | "accessed" | "used" => SortKey::Atime,
            "created" | "btime" | "birth" | "born" => SortKey::Created,
            "size" | "s" | "bytes" => SortKey::Size,
            "ext" | "extension" | "type" | "kind" => SortKey::Ext,
            "random" | "rand" | "shuffle" | "r" => SortKey::Random,
            other => bail!(
                "unknown sort key `{other}`\n\
                 valid keys: name, natural, time, created, ctime, atime, size, ext, random"
            ),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Natural => "natural",
            SortKey::Time => "time",
            SortKey::Ctime => "ctime",
            SortKey::Atime => "atime",
            SortKey::Created => "created",
            SortKey::Size => "size",
            SortKey::Ext => "ext",
            SortKey::Random => "random",
        }
    }

    /// Sort keys where "biggest first" is the intuitive default, so that
    /// `magicfs sort time` shows newest-first without needing `--reverse`.
    pub fn descends_by_default(self) -> bool {
        matches!(
            self,
            SortKey::Time | SortKey::Created | SortKey::Ctime | SortKey::Atime | SortKey::Size
        )
    }
}

/// How directories in the source are treated.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum DirMode {
    /// Directories appear in the view, ordered alongside files.
    #[default]
    Include,
    /// Directories are omitted — `*` expands to files only.
    Exclude,
    /// Only directories appear.
    Only,
}

impl DirMode {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "include" | "yes" | "both" => DirMode::Include,
            "exclude" | "no" | "files" => DirMode::Exclude,
            "only" | "dirs" => DirMode::Only,
            other => bail!("unknown dir mode `{other}` (want: include, exclude, only)"),
        })
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ViewSpec {
    pub sort: SortKey,
    /// Flips whatever direction `sort` implies by default.
    pub reverse: bool,
    /// Frozen so that `magicfs refresh` reproduces the same shuffle; only
    /// `magicfs shuffle` re-rolls it.
    pub seed: u64,
    /// Exactly which entries the view holds, as paths relative to the source.
    ///
    /// Set when a command line named specific files (`magicfs mpv *.mp4`), so
    /// that the view is the files the command asked for and nothing else.
    /// Empty means "everything the filters allow".
    #[serde(default)]
    pub only: Vec<String>,
    /// Include-globs. Empty means "everything".
    pub filter: Vec<String>,
    /// Exclude-globs, applied after `filter`.
    pub exclude: Vec<String>,
    /// Drop files already handed to a command — see [`crate::seen`]. Applied
    /// before `limit`, so "5 unseen" means five you haven't seen.
    #[serde(default)]
    pub unseen: bool,
    /// Only files from this stretch of time — see [`crate::when`]. Kept as
    /// written and re-read on every rebuild, so `today` stays today.
    #[serde(default)]
    pub when: Option<String>,
    /// Read `when` and sessions by creation time rather than modification.
    #[serde(default)]
    pub created: bool,
    /// Keep only the first N entries *after* ordering.
    pub limit: Option<usize>,
    /// Flatten the whole subtree into one directory.
    pub recursive: bool,
    pub dirs: DirMode,
    /// Match globs case-sensitively. Off by default so `png` catches `.PNG`.
    pub case_sensitive: bool,
    pub name_format: String,
    /// Explicit index-prefix width; `None` derives it from the entry count.
    pub pad: Option<usize>,
}

impl Default for ViewSpec {
    fn default() -> Self {
        ViewSpec {
            sort: SortKey::Name,
            reverse: false,
            seed: 0,
            only: Vec::new(),
            filter: Vec::new(),
            exclude: Vec::new(),
            unseen: false,
            when: None,
            created: false,
            limit: None,
            recursive: false,
            dirs: DirMode::default(),
            case_sensitive: false,
            name_format: DEFAULT_NAME_FORMAT.to_string(),
            pad: None,
        }
    }
}

impl ViewSpec {
    /// Whether the ordering runs largest/newest first.
    pub fn descending(&self) -> bool {
        self.sort.descends_by_default() != self.reverse
    }

    /// A one-line human summary, e.g. `time desc, filter *.png, limit 20`.
    pub fn summary(&self) -> String {
        let mut parts = vec![match self.sort {
            SortKey::Random => "random".to_string(),
            k => format!(
                "{} {}",
                k.as_str(),
                if self.descending() { "desc" } else { "asc" }
            ),
        }];
        // The names themselves would be a screenful; the count is the part
        // that tells you the view is narrower than the directory.
        if !self.only.is_empty() {
            parts.push(format!("picked {}", self.only.len()));
        }
        if !self.filter.is_empty() {
            parts.push(format!("filter {}", self.filter.join(",")));
        }
        if !self.exclude.is_empty() {
            parts.push(format!("exclude {}", self.exclude.join(",")));
        }
        if let Some(when) = &self.when {
            let by = if self.created { " (created)" } else { "" };
            parts.push(format!("when {when}{by}"));
        }
        if self.unseen {
            parts.push("unseen".to_string());
        }
        if let Some(n) = self.limit {
            parts.push(format!("limit {n}"));
        }
        if self.recursive {
            parts.push("recursive".to_string());
        }
        match self.dirs {
            DirMode::Include => {}
            DirMode::Exclude => parts.push("files only".to_string()),
            DirMode::Only => parts.push("dirs only".to_string()),
        }
        parts.join(", ")
    }
}

/// A seed for [`SortKey::Random`], derived from the clock and pid.
///
/// Shuffle quality here only needs to beat "looks ordered to a human", so a
/// cheap source is fine — but it is mixed through SplitMix64 so that seeds
/// generated microseconds apart don't produce correlated orderings.
pub fn fresh_seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    mix64(nanos ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

/// SplitMix64 finaliser — used both for seed generation and for deriving a
/// stable per-entry sort value during a random ordering.
pub fn mix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_key_aliases_resolve() {
        assert_eq!(SortKey::parse("mtime").unwrap(), SortKey::Time);
        assert_eq!(SortKey::parse("SHUFFLE").unwrap(), SortKey::Random);
        assert!(SortKey::parse("sideways").is_err());
    }

    #[test]
    fn time_defaults_to_newest_first_and_reverse_flips_it() {
        let mut spec = ViewSpec { sort: SortKey::Time, ..Default::default() };
        assert!(spec.descending());
        spec.reverse = true;
        assert!(!spec.descending());
    }

    #[test]
    fn name_defaults_to_ascending() {
        let spec = ViewSpec::default();
        assert!(!spec.descending());
    }

    #[test]
    fn mix64_decorrelates_adjacent_seeds() {
        // Two seeds one apart must not produce near-identical values, or
        // shuffles created in quick succession would look the same.
        let a = mix64(1);
        let b = mix64(2);
        assert!(a.abs_diff(b) > u64::MAX / 1000);
    }
}
