//! Turning a raw scan into the exact list, in the exact order, that the view
//! should present.

use anyhow::{Context, Result};
use globset::{GlobSet, GlobSetBuilder};
use std::cmp::Ordering;

use crate::entry::Entry;
use crate::spec::{SortKey, ViewSpec, mix64};

/// Named shorthands so `magicfs filter images` does the obvious thing.
const CLASSES: &[(&str, &[&str])] = &[
    (
        "images",
        &["jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff", "avif", "heic", "heif", "svg", "ico"],
    ),
    (
        "raw",
        &["cr2", "cr3", "nef", "arw", "dng", "orf", "raf", "rw2", "pef", "srw"],
    ),
    (
        "video",
        &["mp4", "mkv", "mov", "avi", "webm", "m4v", "mpg", "mpeg", "wmv", "flv"],
    ),
    (
        "audio",
        &["mp3", "flac", "wav", "ogg", "opus", "m4a", "aac", "wma", "aiff"],
    ),
    (
        "docs",
        &["pdf", "epub", "mobi", "djvu", "doc", "docx", "odt", "rtf", "txt", "md"],
    ),
    (
        "archives",
        &["zip", "tar", "gz", "bz2", "xz", "zst", "7z", "rar", "tgz"],
    ),
];

/// Expand a user-supplied filter token into glob patterns.
///
/// The point is that nobody wants to type `'*.png'` (and quote it against the
/// shell) when they mean "just the PNGs". So a bare word is treated as an
/// extension, a known class name expands to its extension set, and anything
/// containing glob metacharacters is passed through untouched.
fn expand_pattern(pat: &str) -> Vec<String> {
    if let Some((_, exts)) = CLASSES.iter().find(|(n, _)| *n == pat.to_ascii_lowercase()) {
        return exts.iter().map(|e| format!("*.{e}")).collect();
    }
    if pat.contains(['*', '?', '[', '{', '/']) {
        return vec![pat.to_string()];
    }
    // `.png` and `png` both mean the extension.
    vec![format!("*.{}", pat.trim_start_matches('.'))]
}

fn build_globset(patterns: &[String], case_sensitive: bool) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for raw in patterns {
        for pat in expand_pattern(raw) {
            let glob = globset::GlobBuilder::new(&pat)
                .case_insensitive(!case_sensitive)
                .build()
                .with_context(|| format!("invalid filter pattern `{raw}`"))?;
            builder.add(glob);
        }
    }
    Ok(Some(builder.build()?))
}

/// Does this entry match the pattern set?
///
/// Patterns with a `/` are matched against the path relative to the source
/// root; everything else matches the bare filename, which is what a user
/// typing `*.png` expects even in recursive mode.
fn matches(set: &GlobSet, entry: &Entry, patterns: &[String]) -> bool {
    let path_scoped = patterns.iter().any(|p| p.contains('/'));
    set.is_match(entry.name.as_str()) || (path_scoped && set.is_match(entry.rel.as_str()))
}

/// Apply the spec's filters, ordering, and limit.
pub fn arrange(mut entries: Vec<Entry>, spec: &ViewSpec) -> Result<Vec<Entry>> {
    // An explicit pick beats every pattern: these are the files a command line
    // named, matched exactly, so a name full of glob metacharacters is safe.
    if !spec.only.is_empty() {
        let wanted: std::collections::HashSet<&str> =
            spec.only.iter().map(String::as_str).collect();
        entries.retain(|e| wanted.contains(e.rel.as_str()));
    }
    if let Some(set) = build_globset(&spec.filter, spec.case_sensitive)? {
        entries.retain(|e| matches(&set, e, &spec.filter));
    }
    if let Some(set) = build_globset(&spec.exclude, spec.case_sensitive)? {
        entries.retain(|e| !matches(&set, e, &spec.exclude));
    }

    sort_entries(&mut entries, spec);

    if let Some(n) = spec.limit {
        entries.truncate(n);
    }
    Ok(entries)
}

fn sort_entries(entries: &mut [Entry], spec: &ViewSpec) {
    let descending = spec.descending();
    entries.sort_by(|a, b| {
        let ord = compare(a, b, spec);
        // Every comparison falls back to the relative path, so the result is a
        // total order and repeated runs over an unchanged directory produce
        // byte-identical views.
        let ord = if ord == Ordering::Equal { a.rel.cmp(&b.rel) } else { ord };
        if descending { ord.reverse() } else { ord }
    });
}

fn compare(a: &Entry, b: &Entry, spec: &ViewSpec) -> Ordering {
    match spec.sort {
        SortKey::Name => a.name.cmp(&b.name),
        SortKey::Natural => natural_cmp(&a.name, &b.name),
        SortKey::Time => a.mtime.cmp(&b.mtime),
        SortKey::Ctime => a.ctime.cmp(&b.ctime),
        SortKey::Atime => a.atime.cmp(&b.atime),
        SortKey::Size => a.size.cmp(&b.size),
        SortKey::Ext => a
            .ext
            .to_ascii_lowercase()
            .cmp(&b.ext.to_ascii_lowercase())
            .then_with(|| natural_cmp(&a.name, &b.name)),
        SortKey::Random => shuffle_key(a, spec.seed).cmp(&shuffle_key(b, spec.seed)),
    }
}

/// A stable pseudo-random ordinal for an entry under a given seed.
///
/// Deriving it from the path rather than shuffling in place means the order
/// depends only on (seed, contents) — so `magicfs refresh` after adding a file
/// leaves the existing files where they were instead of reshuffling the world.
fn shuffle_key(entry: &Entry, seed: u64) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a offset basis
    for byte in entry.rel.as_bytes() {
        h ^= *byte as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    mix64(h ^ seed)
}

/// Compare filenames with runs of digits treated as numbers, so `img2` sorts
/// before `img10` instead of after it.
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut ai, mut bi) = (a.bytes().peekable(), b.bytes().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                if x.is_ascii_digit() && y.is_ascii_digit() {
                    let na = take_number(&mut ai);
                    let nb = take_number(&mut bi);
                    match na.cmp(&nb) {
                        Ordering::Equal => continue,
                        other => return other,
                    }
                }
                match x.cmp(&y) {
                    Ordering::Equal => {
                        ai.next();
                        bi.next();
                    }
                    other => return other,
                }
            }
        }
    }
}

/// Consume a run of digits. Saturating, so a 40-digit filename can't panic.
fn take_number(it: &mut std::iter::Peekable<std::str::Bytes<'_>>) -> u128 {
    let mut n: u128 = 0;
    while let Some(d) = it.peek().copied().filter(u8::is_ascii_digit) {
        n = n.saturating_mul(10).saturating_add((d - b'0') as u128);
        it.next();
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::scan;
    use crate::spec::DirMode;
    use crate::testutil::TempDir;

    fn names(td: &TempDir, spec: &ViewSpec) -> Vec<String> {
        let entries = scan(td.path(), spec).unwrap();
        arrange(entries, spec).unwrap().into_iter().map(|e| e.name).collect()
    }

    #[test]
    fn natural_sort_orders_numbers_numerically() {
        assert_eq!(natural_cmp("img2.jpg", "img10.jpg"), Ordering::Less);
        assert_eq!(natural_cmp("img10.jpg", "img2.jpg"), Ordering::Greater);
        assert_eq!(natural_cmp("a.jpg", "a.jpg"), Ordering::Equal);
        // Zero-padding must not change the numeric comparison.
        assert_eq!(natural_cmp("img007.jpg", "img7.jpg"), Ordering::Equal);
    }

    #[test]
    fn sorts_by_mtime_newest_first_by_default() {
        let td = TempDir::new("order-time");
        td.touch_at("oldest.jpg", 1_000);
        td.touch_at("newest.jpg", 3_000);
        td.touch_at("middle.jpg", 2_000);

        let spec = ViewSpec { sort: SortKey::Time, ..Default::default() };
        assert_eq!(names(&td, &spec), vec!["newest.jpg", "middle.jpg", "oldest.jpg"]);

        let spec = ViewSpec { reverse: true, ..spec };
        assert_eq!(names(&td, &spec), vec!["oldest.jpg", "middle.jpg", "newest.jpg"]);
    }

    #[test]
    fn bare_word_filter_is_treated_as_an_extension() {
        let td = TempDir::new("order-filter");
        td.touch("a.png");
        td.touch("b.jpg");
        td.touch("c.PNG");

        // Bare `png`, dotted `.png` and explicit `*.png` are equivalent, and
        // all of them catch the uppercase file.
        for pat in ["png", ".png", "*.png"] {
            let spec = ViewSpec { filter: vec![pat.into()], ..Default::default() };
            assert_eq!(names(&td, &spec), vec!["a.png", "c.PNG"], "pattern {pat}");
        }
    }

    #[test]
    fn case_sensitive_filtering_can_be_opted_into() {
        let td = TempDir::new("order-case");
        td.touch("a.png");
        td.touch("c.PNG");
        let spec = ViewSpec {
            filter: vec!["png".into()],
            case_sensitive: true,
            ..Default::default()
        };
        assert_eq!(names(&td, &spec), vec!["a.png"]);
    }

    #[test]
    fn class_shorthand_expands_to_many_extensions() {
        let td = TempDir::new("order-class");
        td.touch("a.jpg");
        td.touch("b.webp");
        td.touch("c.txt");
        let spec = ViewSpec { filter: vec!["images".into()], ..Default::default() };
        assert_eq!(names(&td, &spec), vec!["a.jpg", "b.webp"]);
    }

    #[test]
    fn exclude_applies_after_filter() {
        let td = TempDir::new("order-exclude");
        td.touch("keep.jpg");
        td.touch("thumb.jpg");
        let spec = ViewSpec {
            filter: vec!["images".into()],
            exclude: vec!["thumb*".into()],
            ..Default::default()
        };
        assert_eq!(names(&td, &spec), vec!["keep.jpg"]);
    }

    #[test]
    fn limit_applies_after_ordering_not_before() {
        let td = TempDir::new("order-limit");
        td.touch_at("old.jpg", 1_000);
        td.touch_at("new.jpg", 3_000);
        td.touch_at("mid.jpg", 2_000);
        let spec = ViewSpec {
            sort: SortKey::Time,
            limit: Some(2),
            ..Default::default()
        };
        // The 2 *newest*, not the first 2 encountered.
        assert_eq!(names(&td, &spec), vec!["new.jpg", "mid.jpg"]);
    }

    #[test]
    fn shuffle_is_stable_for_a_seed_and_differs_across_seeds() {
        let td = TempDir::new("order-shuffle");
        for i in 0..40 {
            td.touch(&format!("f{i:02}.jpg"));
        }
        let with = |seed| {
            names(&td, &ViewSpec { sort: SortKey::Random, seed, ..Default::default() })
        };
        assert_eq!(with(42), with(42), "same seed must reproduce the order");
        assert_ne!(with(42), with(43), "different seeds must differ");
        assert_ne!(
            with(42),
            names(&td, &ViewSpec::default()),
            "shuffle must not coincide with name order"
        );
    }

    #[test]
    fn adding_a_file_does_not_reshuffle_the_rest() {
        // The whole point of hashing the path instead of shuffling in place:
        // a refresh after dropping in a new photo shouldn't scramble the view.
        let td = TempDir::new("order-shuffle-stable");
        for i in 0..20 {
            td.touch(&format!("f{i:02}.jpg"));
        }
        let spec = ViewSpec { sort: SortKey::Random, seed: 7, ..Default::default() };
        let before = names(&td, &spec);

        td.touch("zz-new.jpg");
        let after: Vec<String> = names(&td, &spec).into_iter().filter(|n| n != "zz-new.jpg").collect();

        assert_eq!(before, after);
    }

    #[test]
    fn path_scoped_globs_match_against_the_relative_path() {
        let td = TempDir::new("order-relglob");
        td.mkdir("2024");
        td.touch("2024/a.jpg");
        td.touch("b.jpg");
        let spec = ViewSpec {
            recursive: true,
            dirs: DirMode::Exclude,
            filter: vec!["2024/*".into()],
            ..Default::default()
        };
        assert_eq!(names(&td, &spec), vec!["a.jpg"]);
    }
}
