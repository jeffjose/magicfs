//! Rendering view filenames.
//!
//! This is the module that actually makes the tool work. The shell sorts glob
//! results itself — `feh *` never sees our readdir order — so the *only* lever
//! we have over the order a tool receives is the names themselves. Every entry
//! therefore gets an index prefix chosen so that byte-wise filename order
//! reproduces the order the user asked for.

use crate::entry::Entry;
use crate::spec::{MIN_PAD, ViewSpec};
use std::collections::HashSet;

/// A single planned view entry: what to call it, and what it points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Named {
    pub name: String,
    pub entry: Entry,
}

/// Width of the index prefix: wide enough that the largest index doesn't
/// overflow it, and never so narrow that the padding stops working.
pub fn pad_width(count: usize, spec: &ViewSpec) -> usize {
    if let Some(p) = spec.pad {
        return p.max(1);
    }
    let digits = count.to_string().len();
    digits.max(MIN_PAD)
}

/// Render the final view names for an already-ordered entry list.
pub fn render(entries: Vec<Entry>, spec: &ViewSpec) -> Vec<Named> {
    let width = pad_width(entries.len(), spec);
    let mut used: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(entries.len());

    for (idx, entry) in entries.into_iter().enumerate() {
        let raw = expand(&spec.name_format, &entry, idx + 1, width);
        let name = deduplicate(sanitize(raw), &mut used);
        out.push(Named { name, entry });
    }
    out
}

fn expand(format: &str, entry: &Entry, index: usize, width: usize) -> String {
    let mut out = String::with_capacity(format.len() + entry.name.len() + width);
    let mut rest = format;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('}').map(|i| open + i) else {
            // Unbalanced brace: emit the remainder literally rather than
            // silently swallowing the rest of the template.
            break;
        };
        let token = &rest[open + 1..close];
        match token {
            "i" => out.push_str(&format!("{index:0width$}")),
            "n" => out.push_str(&index.to_string()),
            "name" => out.push_str(&entry.name),
            "stem" => out.push_str(&entry.stem),
            "ext" => out.push_str(&entry.ext),
            // `/` can't appear in a filename, so flatten it for recursive views.
            "rel" => out.push_str(&entry.rel.replace('/', "~")),
            "size" => out.push_str(&entry.size.to_string()),
            unknown => {
                // Preserve unrecognised tokens verbatim; a typo should be
                // visible in the output rather than vanish.
                out.push('{');
                out.push_str(unknown);
                out.push('}');
            }
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

/// Force the rendered string to be a usable, glob-visible filename.
fn sanitize(name: String) -> String {
    let mut s: String = name
        .chars()
        .map(|c| if c == '/' || c == '\0' { '_' } else { c })
        .collect();
    // A leading dot would hide the entry from `*` in every shell — which would
    // defeat the entire purpose of the view.
    if s.starts_with('.') {
        s.insert(0, '_');
    }
    if s.is_empty() {
        s.push('_');
    }
    s
}

/// Ensure uniqueness for templates that omit `{i}` (where two source files can
/// legitimately render to the same name).
fn deduplicate(name: String, used: &mut HashSet<String>) -> String {
    if used.insert(name.clone()) {
        return name;
    }
    // Insert the disambiguator before the extension so `*.png` still matches.
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name.as_str(), ""),
    };
    for n in 2.. {
        let candidate = format!("{stem}~{n}{ext}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::scan;
    use crate::order::arrange;
    use crate::spec::{DirMode, SortKey};
    use crate::testutil::TempDir;

    fn view_names(td: &TempDir, spec: &ViewSpec) -> Vec<String> {
        let entries = arrange(scan(td.path(), spec).unwrap(), spec).unwrap();
        render(entries, spec).into_iter().map(|n| n.name).collect()
    }

    #[test]
    fn default_format_prefixes_a_padded_index() {
        let td = TempDir::new("name-default");
        td.touch_at("old.jpg", 1_000);
        td.touch_at("new.jpg", 2_000);
        let spec = ViewSpec { sort: SortKey::Time, dirs: DirMode::Exclude, ..Default::default() };
        assert_eq!(view_names(&td, &spec), vec!["001-new.jpg", "002-old.jpg"]);
    }

    /// The load-bearing property of the whole tool: sorting the rendered names
    /// the way a shell would must reproduce the requested order.
    #[test]
    fn lexicographic_name_order_reproduces_the_requested_order() {
        let td = TempDir::new("name-lex");
        // Deliberately name files so alphabetical order fights mtime order.
        td.touch_at("aaa.jpg", 5_000);
        td.touch_at("bbb.jpg", 4_000);
        td.touch_at("ccc.jpg", 3_000);
        td.touch_at("ddd.jpg", 2_000);
        td.touch_at("eee.jpg", 1_000);

        let spec = ViewSpec { sort: SortKey::Time, dirs: DirMode::Exclude, ..Default::default() };
        let rendered = view_names(&td, &spec);

        let mut shell_sorted = rendered.clone();
        shell_sorted.sort(); // what bash/tcsh/zsh do to a glob
        assert_eq!(
            rendered, shell_sorted,
            "glob expansion would reorder the view"
        );
        // And the underlying order really is newest-first.
        assert!(rendered[0].ends_with("aaa.jpg"));
        assert!(rendered[4].ends_with("eee.jpg"));
    }

    #[test]
    fn padding_widens_past_the_minimum_so_order_survives() {
        let td = TempDir::new("name-pad");
        for i in 0..120 {
            td.touch(&format!("f{i:03}.jpg"));
        }
        let spec = ViewSpec { dirs: DirMode::Exclude, ..Default::default() };
        let rendered = view_names(&td, &spec);
        assert_eq!(rendered[0], "001-f000.jpg");
        assert_eq!(rendered[119], "120-f119.jpg");

        let mut sorted = rendered.clone();
        sorted.sort();
        assert_eq!(rendered, sorted);
    }

    #[test]
    fn thousand_entry_view_still_sorts_correctly() {
        // Guards the width calculation at a power-of-ten boundary, where a
        // too-narrow pad would put `1000-` before `999-`.
        let spec = ViewSpec::default();
        assert_eq!(pad_width(1000, &spec), 4);
        let names: Vec<String> = (1..=1000)
            .map(|i| format!("{:0w$}-x.jpg", i, w = pad_width(1000, &spec)))
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    #[test]
    fn custom_templates_expand_tokens() {
        let td = TempDir::new("name-tpl");
        td.touch("photo.jpg");
        let spec = ViewSpec {
            name_format: "{n}_{stem}.{ext}".into(),
            dirs: DirMode::Exclude,
            ..Default::default()
        };
        assert_eq!(view_names(&td, &spec), vec!["1_photo.jpg"]);
    }

    #[test]
    fn unknown_tokens_survive_verbatim() {
        let td = TempDir::new("name-unknown");
        td.touch("photo.jpg");
        let spec = ViewSpec {
            name_format: "{bogus}-{name}".into(),
            dirs: DirMode::Exclude,
            ..Default::default()
        };
        assert_eq!(view_names(&td, &spec), vec!["{bogus}-photo.jpg"]);
    }

    #[test]
    fn recursive_rel_token_flattens_separators() {
        let td = TempDir::new("name-rel");
        td.mkdir("2024");
        td.touch("2024/a.jpg");
        let spec = ViewSpec {
            recursive: true,
            dirs: DirMode::Exclude,
            name_format: "{i}-{rel}".into(),
            ..Default::default()
        };
        assert_eq!(view_names(&td, &spec), vec!["001-2024~a.jpg"]);
    }

    #[test]
    fn templates_without_an_index_still_produce_unique_names() {
        let td = TempDir::new("name-dedup");
        td.mkdir("a");
        td.mkdir("b");
        td.touch("a/same.jpg");
        td.touch("b/same.jpg");
        let spec = ViewSpec {
            recursive: true,
            dirs: DirMode::Exclude,
            name_format: "{name}".into(),
            ..Default::default()
        };
        let mut names = view_names(&td, &spec);
        names.sort();
        // Disambiguated before the extension, so `*.jpg` still catches both.
        assert_eq!(names, vec!["same.jpg", "same~2.jpg"]);
    }

    #[test]
    fn leading_dot_is_defused_so_entries_stay_glob_visible() {
        assert_eq!(sanitize(".hidden.jpg".into()), "_.hidden.jpg");
        assert_eq!(sanitize("a/b.jpg".into()), "a_b.jpg");
        assert_eq!(sanitize(String::new()), "_");
    }
}
