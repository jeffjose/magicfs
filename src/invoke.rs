//! Working out what the words after `magicfs` mean, and turning them into the
//! command line that actually runs.
//!
//! `mfr smplayer *` never reaches us as `smplayer *`: the shell expands the
//! glob first, so we are handed `smplayer a.mp4 b.mp4 c.mp4` — the right files
//! in the wrong order, which is the one thing we cannot use. The fix is to
//! recognise those words as the glob's output, drop them, and put the view's
//! ordered entries back in the same slot. What the command receives is then
//! exactly what `smplayer *` would have expanded to *inside the view*.
//!
//! Quoting sidesteps the whole problem — `mfr smplayer '*.mp4'` arrives
//! unexpanded — so a pattern is matched against the source directory here
//! instead, which lands in the same place.

use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::entry::Entry;

/// What the trailing words are asking for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ask {
    /// Nothing trailing: build or reconfigure the view and stop there.
    View,
    /// Run this command line inside the view.
    Run(Vec<String>),
    /// Files but no command: build a view holding exactly these.
    Pick(Vec<String>),
}

/// Split the trailing words into a source directory and what to do in it.
///
/// The ambiguity is real — `magicfs photos` could be a directory or a program —
/// so the tests are ordered by how confident each one is:
///
///   1. a lone directory is the original `magicfs ~/photos`;
///   2. a word that names an executable is a command;
///   3. words that all name files are a glob expansion, so there is no command;
///   4. a directory in front of the rest is an explicit source.
pub fn interpret(rest: &[String]) -> (Option<PathBuf>, Ask) {
    if rest.is_empty() {
        return (None, Ask::View);
    }
    if rest.len() == 1 && is_dir(&rest[0]) {
        return (Some(PathBuf::from(&rest[0])), Ask::View);
    }
    match tail(rest) {
        // Not a command and not a file list, but it does name a directory:
        // `magicfs ~/photos mpv --loop *`.
        Ask::Run(_) if is_dir(&rest[0]) && !is_program(&rest[0]) => {
            (Some(PathBuf::from(&rest[0])), tail(&rest[1..]))
        }
        ask => (None, ask),
    }
}

fn tail(words: &[String]) -> Ask {
    match words {
        [] => Ask::View,
        [one] if is_shell_line(one) => Ask::Run(words.to_vec()),
        [first, ..] if is_program(first) => Ask::Run(words.to_vec()),
        _ if words.iter().all(|w| names_files(w)) => Ask::Pick(words.to_vec()),
        // Nothing else fits: treat it as a command and let the exec fail with a
        // message naming the program, which is what the user typed.
        _ => Ask::Run(words.to_vec()),
    }
}

/// A command line with the file arguments lifted out of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    /// Every word we are keeping — the program and its own flags — in the
    /// order it was typed.
    pub argv: Vec<String>,
    /// Where in `argv` the view's ordered entries belong. Trailing when the
    /// user named no files at all, so `mfr mpv` still gets the whole view.
    pub at: usize,
    /// Source-relative paths the user singled out. Empty when they named every
    /// entry (or none) — i.e. when the view needs no narrowing.
    pub chosen: Vec<String>,
}

impl Invocation {
    /// The final argv: the kept words with the view's entry names spliced into
    /// the slot the file arguments came out of.
    pub fn with_files<I: IntoIterator<Item = String>>(mut self, files: I) -> Vec<String> {
        let at = self.at;
        self.argv.splice(at..at, files);
        self.argv
    }
}

/// Take the file arguments out of a command line.
///
/// `has_program` marks the first word as the command to run, so that a file
/// that happens to share its name is never mistaken for one of the arguments.
pub fn select(
    words: &[String],
    has_program: bool,
    source: &Path,
    entries: &[Entry],
) -> Result<Invocation> {
    let mut argv = Vec::new();
    let mut chosen = Vec::new();
    let mut seen = HashSet::new();
    let mut at = None;
    let mut canon = None;

    for (i, word) in words.iter().enumerate() {
        // The command's own flags are none of our business — `--loop` means
        // something to mpv and nothing to us, so it survives untouched.
        if (i == 0 && has_program) || is_flag(word) {
            argv.push(word.clone());
            continue;
        }

        let hits = if let Some(rel) = resolve(word, entries, &mut canon) {
            vec![rel]
        } else if is_pattern(word) {
            // Quoted, so the shell left it alone: expand it ourselves.
            let hits = matching(word, entries)?;
            if hits.is_empty() {
                bail!("nothing in {} matches `{word}`", source.display());
            }
            hits
        } else {
            // A destination path, a numeric option value, a URL: not ours.
            argv.push(word.clone());
            continue;
        };

        at.get_or_insert(argv.len());
        for rel in hits {
            if seen.insert(rel.clone()) {
                chosen.push(rel);
            }
        }
    }

    // Naming every file is the `*` case, and narrowing a view to all of it is
    // both pointless and a lot of state to write down.
    if chosen.len() == entries.len() {
        chosen.clear();
    }
    Ok(Invocation { at: at.unwrap_or(argv.len()), argv, chosen })
}

/// The entry a word refers to, if any.
fn resolve(
    word: &str,
    entries: &[Entry],
    canon: &mut Option<HashMap<PathBuf, String>>,
) -> Option<String> {
    // The overwhelmingly common case: `*` expanded in the source directory, so
    // the word is a plain entry name.
    if !word.contains('/')
        && let Some(entry) = entries.iter().find(|e| e.name == word || e.rel == word)
    {
        return Some(entry.rel.clone());
    }
    // Anything else — a path with directories in it, or a link in the view we
    // are already standing in — is matched by where it lands on disk.
    let real = std::fs::canonicalize(word).ok()?;
    let map = canon.get_or_insert_with(|| {
        entries
            .iter()
            .filter_map(|e| Some((std::fs::canonicalize(&e.path).ok()?, e.rel.clone())))
            .collect()
    });
    map.get(&real).cloned()
}

/// Entries a still-unexpanded glob would have matched.
fn matching(pattern: &str, entries: &[Entry]) -> Result<Vec<String>> {
    let glob = globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .with_context(|| format!("`{pattern}` is not a valid pattern"))?
        .compile_matcher();
    let scoped = pattern.contains('/');
    Ok(entries
        .iter()
        .filter(|e| glob.is_match(e.name.as_str()) || (scoped && glob.is_match(e.rel.as_str())))
        .map(|e| e.rel.clone())
        .collect())
}

/// A single argument that is a whole command *line* rather than a program name
/// — `magicfs 'mpv --loop *'`.
///
/// Quoting a whole line is how the user says "leave this alone", so it is run
/// by a shell inside the view with nothing rewritten: the glob expands there,
/// against the view, which is the answer anyway.
pub fn is_shell_line(word: &str) -> bool {
    word.split_whitespace().count() > 1 && !Path::new(word).exists()
}

/// A word the shell would have expanded had it not been quoted.
fn is_pattern(word: &str) -> bool {
    word.contains(['*', '?', '['])
}

fn is_flag(word: &str) -> bool {
    word.len() > 1 && word.starts_with('-')
}

fn names_files(word: &str) -> bool {
    is_pattern(word) || Path::new(word).exists()
}

fn is_dir(word: &str) -> bool {
    Path::new(word).is_dir()
}

/// Whether a word names something we could actually run.
///
/// This is the test that separates `mfr mpv *` from `mfr *`: consulting PATH is
/// exactly what the shell would do with the same word, so the answer matches
/// the user's own reading of what they typed.
fn is_program(word: &str) -> bool {
    if word.contains('/') {
        return is_executable(Path::new(word));
    }
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| is_executable(&dir.join(word)))
    })
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|md| md.is_file() && md.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::scan;
    use crate::spec::ViewSpec;
    use crate::testutil::TempDir;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn fixture(label: &str) -> (TempDir, Vec<Entry>) {
        let td = TempDir::new(label);
        td.touch("a.mp4");
        td.touch("b.mp4");
        td.touch("c.txt");
        let entries = scan(td.path(), &ViewSpec::default()).unwrap();
        (td, entries)
    }

    #[test]
    fn an_expanded_glob_is_removed_and_the_flags_are_not() {
        let (td, entries) = fixture("invoke-expanded");
        let inv = select(
            &words(&["smplayer", "--fullscreen", "a.mp4", "b.mp4", "c.txt"]),
            true,
            td.path(),
            &entries,
        )
        .unwrap();

        assert_eq!(inv.argv, words(&["smplayer", "--fullscreen"]));
        assert_eq!(inv.at, 2);
        // Every entry named, so the view needs no narrowing.
        assert!(inv.chosen.is_empty());
        assert_eq!(
            inv.with_files(words(&["001-b.mp4", "002-a.mp4"])),
            words(&["smplayer", "--fullscreen", "001-b.mp4", "002-a.mp4"])
        );
    }

    #[test]
    fn files_go_back_where_they_were_taken_from() {
        // `cp * /backup` must not become `cp /backup <files>`.
        let (td, entries) = fixture("invoke-position");
        let inv = select(
            &words(&["cp", "a.mp4", "b.mp4", "c.txt", "/backup"]),
            true,
            td.path(),
            &entries,
        )
        .unwrap();
        assert_eq!(inv.argv, words(&["cp", "/backup"]));
        assert_eq!(inv.at, 1);
        assert_eq!(
            inv.with_files(words(&["001-b.mp4"])),
            words(&["cp", "001-b.mp4", "/backup"])
        );
    }

    #[test]
    fn a_partial_glob_narrows_the_view_to_what_it_matched() {
        let (td, entries) = fixture("invoke-subset");
        let inv = select(&words(&["smplayer", "a.mp4", "b.mp4"]), true, td.path(), &entries).unwrap();
        let mut chosen = inv.chosen.clone();
        chosen.sort();
        assert_eq!(chosen, words(&["a.mp4", "b.mp4"]), "c.txt was not asked for");
    }

    #[test]
    fn a_quoted_pattern_is_expanded_against_the_source() {
        let (td, entries) = fixture("invoke-quoted");
        let inv = select(&words(&["smplayer", "*.mp4"]), true, td.path(), &entries).unwrap();
        assert_eq!(inv.argv, words(&["smplayer"]));
        let mut chosen = inv.chosen.clone();
        chosen.sort();
        assert_eq!(chosen, words(&["a.mp4", "b.mp4"]));
    }

    #[test]
    fn a_quoted_pattern_that_matches_nothing_is_an_error() {
        let (td, entries) = fixture("invoke-nomatch");
        let err = select(&words(&["smplayer", "*.flac"]), true, td.path(), &entries).unwrap_err();
        assert!(err.to_string().contains("*.flac"), "got: {err}");
    }

    #[test]
    fn a_command_with_no_files_still_gets_the_whole_view() {
        let (td, entries) = fixture("invoke-bare");
        let inv = select(&words(&["mpv", "--loop"]), true, td.path(), &entries).unwrap();
        assert_eq!(inv.at, 2, "the files belong after the flags");
        assert_eq!(
            inv.with_files(words(&["001-a.mp4"])),
            words(&["mpv", "--loop", "001-a.mp4"])
        );
    }

    #[test]
    fn the_program_is_never_read_as_one_of_its_own_arguments() {
        let td = TempDir::new("invoke-namesake");
        td.touch("mpv");
        td.touch("a.mp4");
        let entries = scan(td.path(), &ViewSpec::default()).unwrap();

        let inv = select(&words(&["mpv", "a.mp4"]), true, td.path(), &entries).unwrap();
        assert_eq!(inv.argv, words(&["mpv"]));
        assert_eq!(inv.chosen, words(&["a.mp4"]));
    }

    #[test]
    fn a_lone_directory_is_the_source() {
        let td = TempDir::new("invoke-source");
        td.mkdir("photos");
        let dir = td.path().join("photos").to_string_lossy().into_owned();
        assert_eq!(interpret(&words(&[&dir])), (Some(PathBuf::from(&dir)), Ask::View));
    }

    #[test]
    fn an_executable_first_word_is_a_command() {
        assert_eq!(
            interpret(&words(&["true", "x.mp4"])),
            (None, Ask::Run(words(&["true", "x.mp4"])))
        );
    }

    #[test]
    fn a_bare_glob_expansion_is_a_file_list_not_a_command() {
        // `mfr *` in a directory whose first entry happens to be a directory
        // must not read that entry as the source.
        let td = TempDir::new("invoke-bareglob");
        td.mkdir("archive");
        td.touch("b.mp4");
        let at = |n: &str| td.path().join(n).to_string_lossy().into_owned();
        let expanded = words(&[&at("archive"), &at("b.mp4")]);

        assert_eq!(interpret(&expanded), (None, Ask::Pick(expanded.clone())));
    }

    #[test]
    fn a_source_directory_can_be_followed_by_a_command() {
        let td = TempDir::new("invoke-src-cmd");
        td.mkdir("photos");
        let dir = td.path().join("photos").to_string_lossy().into_owned();
        assert_eq!(
            interpret(&words(&[&dir, "true", "--flag"])),
            (Some(PathBuf::from(&dir)), Ask::Run(words(&["true", "--flag"])))
        );
    }

    #[test]
    fn an_unknown_program_stays_a_command_so_the_error_names_it() {
        assert_eq!(
            interpret(&words(&["definitely-not-installed", "--x"])),
            (None, Ask::Run(words(&["definitely-not-installed", "--x"])))
        );
    }
}
