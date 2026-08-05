//! Command-line surface.

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

use crate::spec::{DirMode, SortKey, ViewSpec, fresh_seed};

const ABOUT: &str = "Present a directory in whatever order you want, so `*` expands to it.";

const AFTER_HELP: &str = "\
The shell sorts glob results itself, so magicfs gives each file an index
prefix (001-, 002-, ...) chosen so that alphabetical order *is* your order.

  magicfs ~/photos -s time        build a newest-first view and print its path
  cd \"$(magicfs ~/photos -s time)\"
  magicfs shuffle                 reshuffle the view you are standing in
  magicfs filter png              restrict it to PNGs
  feh *                           opens in the view's order

To keep the original filenames, skip the view and hand the tool an ordered
argument list instead:

  magicfs exec -s random feh      runs: feh /photos/c.jpg /photos/a.jpg ...
  magicfs paths -s time | feh -f -
";

#[derive(Parser, Debug)]
#[command(name = "magicfs", version, about = ABOUT, after_help = AFTER_HELP)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Directory to present (default: the current directory).
    pub source: Option<PathBuf>,

    #[command(flatten)]
    pub spec: SpecArgs,

    /// Put the view here instead of under the runtime directory.
    #[arg(long, value_name = "DIR")]
    pub out: Option<PathBuf>,

    /// Start a shell inside the view. Exit it to return where you were.
    ///
    /// A process cannot change its parent's directory, so this is the only way
    /// to land in the view without the `shell-init` wrapper.
    #[arg(long)]
    pub shell: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Reorder the current view: name, natural, time, ctime, atime, size, ext, random.
    Sort {
        key: String,
        #[arg(short, long)]
        reverse: bool,
    },
    /// Re-roll the random order of the current view.
    Shuffle,
    /// Flip the current view's order.
    Reverse,
    /// Restrict the view (e.g. `png`, `images`, `'IMG_*'`). No arguments clears it.
    Filter { patterns: Vec<String> },
    /// Drop matching entries from the view. No arguments clears it.
    Exclude { patterns: Vec<String> },
    /// Keep only the first N entries. `none` removes the limit.
    Limit { n: String },
    /// Reset filters and limit, keeping the ordering.
    Clear,
    /// Rebuild the view from the source directory.
    Refresh,
    /// Show the current view's configuration.
    Status,
    /// List every view that exists.
    List,
    /// Remove a view. Only ever deletes links, never the real files.
    Close {
        /// Close every view.
        #[arg(long)]
        all: bool,
    },
    /// Run a command with the ordered files as arguments, keeping their real names.
    Exec {
        #[command(flatten)]
        spec: SpecArgs,
        /// Print the command instead of running it.
        #[arg(long)]
        dry_run: bool,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Print the ordered real paths, one per line.
    Paths {
        #[command(flatten)]
        spec: SpecArgs,
        /// Separate with NUL instead of newline, for `xargs -0`.
        #[arg(short = '0', long)]
        print0: bool,
    },
    /// Print the real path behind a view entry.
    Which { name: String },
    /// Create a throwaway directory of sample files for trying magicfs out.
    Demo {
        /// How many files to create.
        #[arg(short = 'n', long, default_value_t = 10)]
        count: usize,
        /// Where to put it (default: $TMPDIR/magicfs-demo).
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
        /// Build a view of it and start a shell inside.
        #[arg(long)]
        shell: bool,
    },
    /// Emit a shell wrapper that cds into views automatically.
    ShellInit {
        /// bash, zsh, fish, or tcsh. Guessed from $SHELL when omitted.
        shell: Option<String>,
    },
}

/// Ordering/filtering options, shared by the root command and by `exec`/`paths`.
#[derive(Args, Clone, Debug, Default)]
pub struct SpecArgs {
    /// Ordering: name, natural, time, ctime, atime, size, ext, random.
    #[arg(short = 's', long, value_name = "KEY")]
    pub sort: Option<String>,

    /// Reverse the ordering.
    #[arg(short = 'r', long)]
    pub reverse: bool,

    /// Only include matching entries. Repeatable.
    #[arg(short = 'f', long, value_name = "PATTERN")]
    pub filter: Vec<String>,

    /// Drop matching entries. Repeatable.
    #[arg(short = 'x', long, value_name = "PATTERN")]
    pub exclude: Vec<String>,

    /// Keep only the first N entries after ordering.
    #[arg(short = 'n', long, value_name = "N")]
    pub limit: Option<usize>,

    /// Flatten the whole subtree into one directory.
    #[arg(short = 'R', long)]
    pub recursive: bool,

    /// How to treat directories: include, exclude, only.
    #[arg(long, value_name = "MODE")]
    pub dirs: Option<String>,

    /// Name template, e.g. '{i}-{name}' or '{i}.{ext}'.
    #[arg(long, value_name = "TEMPLATE")]
    pub name_format: Option<String>,

    /// Width of the index prefix (default: derived from the entry count).
    #[arg(long, value_name = "N")]
    pub pad: Option<usize>,

    /// Match filters case-sensitively.
    #[arg(long)]
    pub case_sensitive: bool,
}

impl SpecArgs {
    /// True when the user supplied nothing — used to tell "reconfigure this
    /// view" apart from "just show me the view".
    pub fn is_empty(&self) -> bool {
        self.sort.is_none()
            && !self.reverse
            && self.filter.is_empty()
            && self.exclude.is_empty()
            && self.limit.is_none()
            && !self.recursive
            && self.dirs.is_none()
            && self.name_format.is_none()
            && self.pad.is_none()
            && !self.case_sensitive
    }

    /// Layer these options onto an existing spec.
    ///
    /// Only options the user actually typed take effect, so
    /// `magicfs filter png` on a time-sorted view keeps the time sort.
    pub fn apply_to(&self, spec: &mut ViewSpec) -> Result<()> {
        if let Some(key) = &self.sort {
            let key = SortKey::parse(key)?;
            // A fresh `--sort` also resets direction, so switching sort keys
            // doesn't silently inherit a reverse from a previous command.
            spec.reverse = self.reverse;
            if key == SortKey::Random && spec.sort != SortKey::Random {
                spec.seed = fresh_seed();
            }
            spec.sort = key;
        } else if self.reverse {
            spec.reverse = true;
        }

        if !self.filter.is_empty() {
            spec.filter = self.filter.clone();
        }
        if !self.exclude.is_empty() {
            spec.exclude = self.exclude.clone();
        }
        if let Some(n) = self.limit {
            spec.limit = Some(n);
        }
        if self.recursive {
            spec.recursive = true;
        }
        if let Some(mode) = &self.dirs {
            spec.dirs = DirMode::parse(mode)?;
        }
        if let Some(tpl) = &self.name_format {
            spec.name_format = tpl.clone();
        }
        if let Some(pad) = self.pad {
            spec.pad = Some(pad);
        }
        if self.case_sensitive {
            spec.case_sensitive = true;
        }
        Ok(())
    }

    /// Build a spec from scratch with these options applied.
    pub fn to_spec(&self) -> Result<ViewSpec> {
        let mut spec = ViewSpec { seed: fresh_seed(), ..Default::default() };
        self.apply_to(&mut spec)?;
        Ok(spec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn root_command_takes_a_source_and_options() {
        let cli = Cli::try_parse_from(["magicfs", "/photos", "-s", "time", "-f", "png"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.source, Some(PathBuf::from("/photos")));
        assert_eq!(cli.spec.sort.as_deref(), Some("time"));
        assert_eq!(cli.spec.filter, vec!["png".to_string()]);
    }

    #[test]
    fn subcommands_win_over_the_positional_source() {
        let cli = Cli::try_parse_from(["magicfs", "shuffle"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Shuffle)));
        assert_eq!(cli.source, None);
    }

    #[test]
    fn exec_captures_the_whole_trailing_command() {
        let cli = Cli::try_parse_from([
            "magicfs", "exec", "-s", "random", "feh", "--scale-down", "-Z",
        ])
        .unwrap();
        let Some(Command::Exec { spec, command, .. }) = cli.command else {
            panic!("expected exec")
        };
        assert_eq!(spec.sort.as_deref(), Some("random"));
        // The tool's own flags must survive intact rather than being parsed
        // as magicfs options.
        assert_eq!(command, vec!["feh", "--scale-down", "-Z"]);
    }

    #[test]
    fn changing_sort_key_resets_direction() {
        let mut spec = ViewSpec { sort: SortKey::Time, reverse: true, ..Default::default() };
        SpecArgs { sort: Some("name".into()), ..Default::default() }
            .apply_to(&mut spec)
            .unwrap();
        assert_eq!(spec.sort, SortKey::Name);
        assert!(!spec.reverse, "stale reverse must not carry over");
    }

    #[test]
    fn unspecified_options_leave_the_spec_alone() {
        let mut spec = ViewSpec {
            sort: SortKey::Time,
            limit: Some(5),
            recursive: true,
            ..Default::default()
        };
        SpecArgs { filter: vec!["png".into()], ..Default::default() }
            .apply_to(&mut spec)
            .unwrap();
        assert_eq!(spec.sort, SortKey::Time);
        assert_eq!(spec.limit, Some(5));
        assert!(spec.recursive);
        assert_eq!(spec.filter, vec!["png".to_string()]);
    }

    #[test]
    fn switching_to_random_seeds_a_new_shuffle() {
        let mut spec = ViewSpec { sort: SortKey::Name, seed: 0, ..Default::default() };
        SpecArgs { sort: Some("random".into()), ..Default::default() }
            .apply_to(&mut spec)
            .unwrap();
        assert_ne!(spec.seed, 0, "a fresh random sort needs a real seed");
    }

    #[test]
    fn re_applying_random_keeps_the_existing_seed() {
        // `magicfs sort random` twice should not silently reshuffle; only
        // `magicfs shuffle` re-rolls.
        let mut spec = ViewSpec { sort: SortKey::Random, seed: 99, ..Default::default() };
        SpecArgs { sort: Some("random".into()), ..Default::default() }
            .apply_to(&mut spec)
            .unwrap();
        assert_eq!(spec.seed, 99);
    }

    #[test]
    fn is_empty_detects_a_bare_invocation() {
        assert!(SpecArgs::default().is_empty());
        assert!(!SpecArgs { reverse: true, ..Default::default() }.is_empty());
    }
}
