//! Command-line surface.

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use std::collections::HashSet;
use std::path::PathBuf;

use crate::spec::{DirMode, SortKey, ViewSpec, fresh_seed};

const ABOUT: &str = "Present a directory in whatever order you want, so `*` expands to it.";

const AFTER_HELP: &str = "\
The shell sorts glob results itself, so magicfs gives each file an index
prefix (001-, 002-, ...) chosen so that alphabetical order *is* your order.

  cd ~/photos
  magicfs -s random               puts you in a shuffled view of it
  feh *                           opens in that order
  magicfs -s time                 reorder without leaving
  magicfs filter png              restrict it to PNGs
  magicfs close                   back to the real directory

Trailing words are a command to run in the view, and you do not have to
quote the glob — the filenames the shell expanded it to are recognised and
replaced with the view's, in order:

  magicfs -s random feh *         runs feh on a shuffled ~/photos
  magicfs -s random feh -Z *.png  the flag is kept, the PNGs alone are used
  magicfs -s time *.jpg           no command: a view of just the JPEGs

Words that name no file are left alone, so `magicfs -s time -n 3 cp * /backup`
still copies to /backup. File tools (rm, mv, cp, ...) get the real paths,
not the view's names, and anything that deletes asks first (-y skips it).
A single quoted argument is handed to a shell inside the view instead:
magicfs -s random 'mpv --loop *'.

At a terminal, a command that lands somewhere moves you there — via the
`shell-init` wrapper if you installed one, otherwise by starting a subshell
you leave with `exit`. Redirected or in `$(...)`, it just prints the path.
`--no-cd` (or MAGICFS_NO_CD=1) turns that off; `--shell` forces it.

To review a directory that keeps filling up, `--unseen` shows only files
no command has been handed yet, and marks the ones it hands over:

  magicfs -s time -r --unseen -n 5 smplayer *    the next 5 new, oldest first
  magicfs unsee --last                           put the last batch back

When nothing is new, the command is not run and magicfs exits 1.

To keep the original filenames, skip the view and hand the tool an ordered
argument list instead:

  magicfs exec -s random feh      runs: feh /photos/c.jpg /photos/a.jpg ...
  magicfs paths -s time | feh -f -
";

#[derive(Parser, Debug)]
#[command(name = "magicfs", version, about = ABOUT, after_help = AFTER_HELP)]
// Not `args_conflicts_with_subcommands`: that would reject the global
// --shell/--no-cd flags when they appear before a subcommand.
#[command(subcommand_negates_reqs = true)]
// The source directory and the trailing command never reach clap — `split_argv`
// takes them out first — so the usage line has to name them itself.
#[command(override_usage = "magicfs [OPTIONS] [SOURCE] [COMMAND...]\n       \
                            magicfs <SUBCOMMAND> [ARGS]")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[command(flatten)]
    pub spec: SpecArgs,

    /// Put the view here instead of under the runtime directory.
    #[arg(long, value_name = "DIR")]
    pub out: Option<PathBuf>,

    /// Print the command that would run instead of running it.
    #[arg(long)]
    pub dry_run: bool,

    /// Hand the command real paths, as `rm`, `mv` and `cp` get anyway.
    #[arg(long, conflicts_with = "links")]
    pub real: bool,

    /// Hand the command the view's names, even if it is `cp` or `tar`.
    #[arg(long)]
    pub links: bool,

    /// Don't ask before `rm` and the like.
    #[arg(short = 'y', long)]
    pub yes: bool,

    /// Open a second view instead of reconfiguring the one you're in.
    ///
    /// Useful for holding two orderings of the same directory at once —
    /// `magicfs -s random --new` twice gives you two shuffles to compare.
    #[arg(long, global = true)]
    pub new: bool,

    /// Stay put: build the view, print its path, and don't move the shell.
    #[arg(long, global = true)]
    pub no_cd: bool,

    /// Always start a shell inside the view, even when output is redirected.
    ///
    /// A process cannot change its parent's directory, so a subshell is the
    /// only way to land in the view without the `shell-init` wrapper. At a
    /// terminal this already happens by default; the flag forces it.
    #[arg(long, global = true, conflicts_with = "no_cd")]
    pub shell: bool,
}

/// Whether a command should move the user to the directory it produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CdPref {
    /// Move them by whatever means works: the `shell-init` wrapper if it is
    /// installed, otherwise a subshell — but only at a terminal, so capturing
    /// our stdout still just yields a path.
    Auto,
    /// Force the subshell even when output is redirected.
    Subshell,
    Never,
}

/// Parse the real command line: our own options, and the words after them.
pub fn parse() -> (Cli, Vec<String>) {
    let (options, rest) = split_argv(std::env::args());
    (Cli::parse_from(options), rest)
}

/// Separate magicfs's own options from the trailing words — the source
/// directory, the command to run in the view, or both.
///
/// clap can't do this itself: in `magicfs -s random mpv --loop *` it has no way
/// to know that `--loop` belongs to mpv, and `trailing_var_arg` overshoots by
/// also claiming the options in `magicfs ~/photos -s time`, which have always
/// meant *our* sort key. So the split happens first and the trailing words
/// never reach the parser at all.
///
/// Which options take a value is read back out of the parser itself, so this
/// cannot drift away from the flags defined above.
pub fn split_argv<I: IntoIterator<Item = String>>(argv: I) -> (Vec<String>, Vec<String>) {
    use clap::CommandFactory;

    let command = Cli::command();
    let mut long_values: HashSet<String> = HashSet::new();
    let mut short_values: HashSet<char> = HashSet::new();
    for arg in command.get_arguments() {
        if !arg.get_action().takes_values() {
            continue;
        }
        long_values.extend(arg.get_long().map(str::to_string));
        long_values.extend(arg.get_all_aliases().unwrap_or_default().iter().map(|a| a.to_string()));
        short_values.extend(arg.get_short());
    }
    let subcommands: HashSet<String> = command
        .get_subcommands()
        .flat_map(|s| {
            std::iter::once(s.get_name().to_string())
                .chain(s.get_all_aliases().map(str::to_string))
        })
        .collect();

    let args: Vec<String> = argv.into_iter().collect();
    let mut out: Vec<String> = args.iter().take(1).cloned().collect();
    let words = &args[out.len().min(args.len())..];

    // The source directory, when it turned up before some of our options.
    let mut held: Option<String> = None;
    let mut trailing: &[String] = &[];

    let mut i = 0;
    while i < words.len() {
        let word = words[i].as_str();
        // An explicit `--` says what we are about to say anyway, and it is how
        // you run a command whose name collides with a subcommand.
        if word == "--" {
            trailing = &words[i + 1..];
            break;
        }
        if let Some(long) = word.strip_prefix("--") {
            out.push(word.to_string());
            i += 1;
            // `--sort=random` carries its own value; `--sort random` eats the
            // next word, which must not be mistaken for the command.
            if !long.contains('=') && long_values.contains(long) && i < words.len() {
                out.push(words[i].clone());
                i += 1;
            }
            continue;
        }
        if word.starts_with('-') && word.len() > 1 {
            out.push(word.to_string());
            i += 1;
            if wants_next_word(word, &short_values) && i < words.len() {
                out.push(words[i].clone());
                i += 1;
            }
            continue;
        }
        // A subcommand parses its own arguments, so hand over the lot.
        if subcommands.contains(word) {
            out.extend(words[i..].iter().cloned());
            return (out, Vec::new());
        }
        // A bare word with more of our options behind it can only be the
        // source directory: `magicfs ~/photos -s time`. Anything else starts
        // the trailing words, which we no longer look inside.
        let more_options = words.get(i + 1).is_some_and(|w| w.starts_with('-'));
        if held.is_none() && more_options && std::path::Path::new(word).is_dir() {
            held = Some(word.to_string());
            i += 1;
            continue;
        }
        trailing = &words[i..];
        break;
    }

    let mut rest: Vec<String> = held.into_iter().collect();
    rest.extend(trailing.iter().cloned());
    (out, rest)
}

/// Whether a short-option cluster still needs the following word as its value.
///
/// `-s time` does; `-stime` and `-rs time`'s leading `-r` do not.
fn wants_next_word(word: &str, short_values: &HashSet<char>) -> bool {
    let mut chars = word.chars().skip(1);
    while let Some(c) = chars.next() {
        if short_values.contains(&c) {
            return chars.next().is_none();
        }
    }
    false
}

impl Cli {
    pub fn cd_pref(&self) -> CdPref {
        if self.shell {
            CdPref::Subshell
        } else if self.no_cd || std::env::var_os("MAGICFS_NO_CD").is_some() {
            CdPref::Never
        } else {
            CdPref::Auto
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Reorder the current view: name, natural, time, ctime, atime, size, ext, random.
    Sort {
        key: String,
        #[arg(short, long)]
        reverse: bool,
    },
    /// Flip the current view's order.
    Reverse,
    /// Restrict the view (e.g. `png`, `images`, `'IMG_*'`). No arguments clears it.
    Filter { patterns: Vec<String> },
    /// Drop matching entries from the view. No arguments clears it.
    Exclude { patterns: Vec<String> },
    /// Keep only the first N entries. `none` removes the limit.
    Limit { n: String },
    /// Reset filters, limit and --unseen, keeping the ordering.
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
    /// Remove every view and leftover directory. Only ever deletes links.
    Clean {
        /// Actually do it. Without this, `clean` only reports what it would remove.
        #[arg(long)]
        yes: bool,
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
    /// Mark files as seen, so `--unseen` skips them. No files: report what's seen.
    Seen { files: Vec<String> },
    /// Make files count as unseen again.
    Unsee {
        /// Undo the most recent marking — for a batch you didn't finish.
        #[arg(long, conflicts_with = "all")]
        last: bool,
        /// Forget everything seen in this directory.
        #[arg(long)]
        all: bool,
        files: Vec<String>,
    },
    /// Create a throwaway directory of sample files for trying magicfs out.
    Demo {
        /// How many files to create.
        #[arg(short = 'n', long, default_value_t = 10)]
        count: usize,
        /// Where to put it (default: $TMPDIR/magicfs-demo).
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
    },
    /// Emit a shell wrapper that cds into views automatically.
    ShellInit {
        /// bash, zsh, fish, or tcsh. Guessed from $SHELL when omitted.
        // Not `shell`: that id belongs to the global --shell flag.
        #[arg(value_name = "SHELL")]
        name: Option<String>,
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

    /// Only files from a stretch of time: today, yesterday.morning, 3h,
    /// mon..wed, 14:00..16:30. `all` clears it.
    #[arg(short = 'w', long, value_name = "WINDOW")]
    pub when: Option<String>,

    /// Newest first, and only the newest unless -n says how many.
    #[arg(long, conflicts_with_all = ["sort", "oldest"])]
    pub latest: bool,

    /// Oldest first, and only the oldest unless -n says how many.
    #[arg(long, conflicts_with = "sort")]
    pub oldest: bool,

    /// Only files no command has been handed yet; a command marks what it gets.
    #[arg(short = 'u', long)]
    pub unseen: bool,

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
            && !self.unseen
            && self.when.is_none()
            && !self.latest
            && !self.oldest
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
            // Asking for random again means "shuffle again" — that is the only
            // thing a second `-s random` could usefully mean. Everything else
            // that rebuilds a view (`refresh`, `filter`, ...) keeps the seed,
            // so an existing shuffle survives adding a file to the directory.
            if key == SortKey::Random {
                spec.seed = fresh_seed();
            }
            spec.sort = key;
        } else if self.latest || self.oldest {
            // `-r` on top flips it, as it would `-s time`.
            spec.sort = SortKey::Time;
            spec.reverse = self.oldest != self.reverse;
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
        } else if self.latest || self.oldest {
            spec.limit = Some(1);
        }
        if self.unseen {
            spec.unseen = true;
        }
        if let Some(when) = &self.when {
            spec.when = match when.as_str() {
                "all" | "any" | "none" | "off" => None,
                text => {
                    // Checked now, so a typo is reported before anything is built.
                    crate::when::Window::parse(text, crate::when::now())?;
                    Some(text.to_string())
                }
            };
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

    /// Catches clashes clap only discovers at runtime — notably a global flag
    /// and a subcommand argument sharing an id, which panics mid-parse.
    #[test]
    fn command_definition_is_internally_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    /// Parse the way the binary does: split first, then hand clap its half.
    fn parse_from(argv: &[&str]) -> (Cli, Vec<String>) {
        let (options, rest) = split_argv(argv.iter().map(|s| s.to_string()));
        (Cli::try_parse_from(options).unwrap(), rest)
    }

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn root_command_takes_a_source_and_options() {
        // The source turns up before our own options and must not swallow them.
        let (cli, rest) = parse_from(&["magicfs", "/tmp", "-s", "time", "-f", "png"]);
        assert!(cli.command.is_none());
        assert_eq!(rest, words(&["/tmp"]));
        assert_eq!(cli.spec.sort.as_deref(), Some("time"));
        assert_eq!(cli.spec.filter, vec!["png".to_string()]);
    }

    #[test]
    fn subcommands_win_over_the_positional_source() {
        let (cli, rest) = parse_from(&["magicfs", "reverse"]);
        assert!(matches!(cli.command, Some(Command::Reverse)));
        assert!(rest.is_empty());
    }

    #[test]
    fn a_trailing_command_keeps_its_own_flags() {
        // The whole point: `--loop` belongs to mpv, `-s` belonged to us.
        let (cli, rest) = parse_from(&["magicfs", "-s", "random", "mpv", "--loop", "a.mp4"]);
        assert_eq!(cli.spec.sort.as_deref(), Some("random"));
        assert_eq!(rest, words(&["mpv", "--loop", "a.mp4"]));
    }

    #[test]
    fn a_source_can_come_before_both_our_options_and_the_command() {
        let (cli, rest) = parse_from(&["magicfs", "/tmp", "-s", "random", "mpv", "*.mp4"]);
        assert_eq!(cli.spec.sort.as_deref(), Some("random"));
        assert_eq!(rest, words(&["/tmp", "mpv", "*.mp4"]));
    }

    #[test]
    fn a_double_dash_escapes_a_command_named_like_a_subcommand() {
        let (cli, rest) = parse_from(&["magicfs", "--", "list", "-l"]);
        assert!(cli.command.is_none(), "`list` after -- is a command, not our subcommand");
        assert_eq!(rest, words(&["list", "-l"]));
    }

    #[test]
    fn attached_option_values_do_not_eat_the_command() {
        for argv in [
            &["magicfs", "--sort=random", "mpv"][..],
            &["magicfs", "-srandom", "mpv"][..],
            &["magicfs", "-rs", "random", "mpv"][..],
        ] {
            let (cli, rest) = parse_from(argv);
            assert_eq!(cli.spec.sort.as_deref(), Some("random"), "{argv:?}");
            assert_eq!(rest, words(&["mpv"]), "{argv:?}");
        }
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
    fn latest_and_oldest_are_a_time_sort_and_a_limit() {
        let spec = SpecArgs { latest: true, ..Default::default() }.to_spec().unwrap();
        assert_eq!((spec.sort, spec.descending(), spec.limit), (SortKey::Time, true, Some(1)));

        let spec = SpecArgs { oldest: true, limit: Some(3), ..Default::default() }
            .to_spec()
            .unwrap();
        assert_eq!((spec.sort, spec.descending(), spec.limit), (SortKey::Time, false, Some(3)));

        assert!(Cli::try_parse_from(["magicfs", "--latest", "--oldest"]).is_err());
        assert!(Cli::try_parse_from(["magicfs", "--latest", "-s", "name"]).is_err());
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
    fn re_applying_random_reshuffles() {
        // There is no `shuffle` subcommand: a second `-s random` is the reshuffle.
        let mut spec = ViewSpec { sort: SortKey::Random, seed: 99, ..Default::default() };
        SpecArgs { sort: Some("random".into()), ..Default::default() }
            .apply_to(&mut spec)
            .unwrap();
        assert_ne!(spec.seed, 99, "a repeated `-s random` should re-roll");
    }

    #[test]
    fn rebuilding_without_a_sort_key_preserves_the_shuffle() {
        // `magicfs filter png` on a shuffled view must not scramble it.
        let mut spec = ViewSpec { sort: SortKey::Random, seed: 99, ..Default::default() };
        SpecArgs { filter: vec!["png".into()], ..Default::default() }
            .apply_to(&mut spec)
            .unwrap();
        assert_eq!(spec.seed, 99);
    }

    #[test]
    fn source_is_optional_so_cwd_is_implied() {
        let (cli, rest) = parse_from(&["magicfs", "--sort=random"]);
        assert!(cli.command.is_none());
        assert!(rest.is_empty());
        assert_eq!(cli.spec.sort.as_deref(), Some("random"));
    }

    #[test]
    fn shell_flags_reach_subcommands_and_are_mutually_exclusive() {
        let cli = Cli::try_parse_from(["magicfs", "--no-cd", "sort", "time"]).unwrap();
        assert_eq!(cli.cd_pref(), CdPref::Never);
        assert!(Cli::try_parse_from(["magicfs", "--shell", "--no-cd"]).is_err());
    }

    #[test]
    fn is_empty_detects_a_bare_invocation() {
        assert!(SpecArgs::default().is_empty());
        assert!(!SpecArgs { reverse: true, ..Default::default() }.is_empty());
    }
}
