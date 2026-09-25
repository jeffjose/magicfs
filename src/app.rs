//! Command implementations.
//!
//! Output convention: the view's path is the only thing written to stdout, so
//! `cd "$(magicfs ~/photos -s time)"` works. Everything else goes to stderr.

use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::cli::{Cli, Command, CdPref, SpecArgs};
use crate::entry::{Entry, scan};
use crate::invoke::{self, Ask};
use crate::links;
use crate::naming::{Named, render};
use crate::order::arrange;
use crate::seen::{self, Seen};
use crate::spec::{ViewSpec, fresh_seed};
use crate::view::{self, View};

/// `rest` is everything after our own options: the source directory, the
/// command to run in the view, or both — see [`crate::cli::split_argv`].
pub fn run(cli: Cli, rest: &[String]) -> Result<()> {
    let pref = cli.cd_pref();
    let new = cli.new;
    match cli.command {
        None => {
            let (source, ask) = invoke::interpret(rest);
            root(source, cli.spec, cli.out, pref, new, ask, cli.dry_run)
        }
        Some(Command::Sort { key, reverse }) => reconfigure(
            pref,
            new,
            SpecArgs { sort: Some(key), reverse, ..Default::default() },
        ),
        Some(Command::Reverse) => mutate(pref, new, |spec| {
            spec.reverse = !spec.reverse;
            Ok(())
        }),
        Some(Command::Filter { patterns }) => mutate(pref, new, move |spec| {
            spec.filter = patterns.clone();
            Ok(())
        }),
        Some(Command::Exclude { patterns }) => mutate(pref, new, move |spec| {
            spec.exclude = patterns.clone();
            Ok(())
        }),
        Some(Command::Limit { n }) => mutate(pref, new, move |spec| {
            spec.limit = match n.as_str() {
                "none" | "off" | "all" | "0" => None,
                other => Some(
                    other
                        .parse()
                        .with_context(|| format!("`{other}` is not a number (or `none`)"))?,
                ),
            };
            Ok(())
        }),
        Some(Command::Clear) => mutate(pref, new, |spec| {
            spec.only.clear();
            spec.filter.clear();
            spec.exclude.clear();
            spec.unseen = false;
            spec.limit = None;
            Ok(())
        }),
        Some(Command::Refresh) => mutate(pref, new, |_| Ok(())),
        Some(Command::Status) => status(),
        Some(Command::List) => list(),
        Some(Command::Close { all }) => close(all),
        Some(Command::Clean { yes }) => clean(yes),
        Some(Command::Exec { spec, dry_run, command }) => exec(spec, dry_run, &command),
        Some(Command::Paths { spec, print0 }) => paths(spec, print0),
        Some(Command::Which { name }) => which(&name),
        Some(Command::Seen { files }) => mark_seen(&files),
        Some(Command::Unsee { last, all, files }) => unsee(last, all, &files),
        Some(Command::Demo { count, out }) => demo(count, out, pref),
        Some(Command::ShellInit { name }) => {
            print!("{}", crate::shellinit::script(name.as_deref())?);
            Ok(())
        }
    }
}

/// Resolve a spec into an ordered, named plan.
pub fn build_plan(source: &Path, spec: &ViewSpec) -> Result<Vec<Named>> {
    let mut entries = scan(source, spec)?;
    // Before `arrange`, so the limit counts only files you haven't seen.
    if spec.unseen {
        let seen = Seen::load(source)?;
        entries.retain(|e| !seen.contains(e));
    }
    Ok(render(arrange(entries, spec)?, spec))
}

/// `magicfs [SOURCE] [OPTIONS] [COMMAND...]` — create a view, or reconfigure
/// the one we're standing in, and optionally run something in it.
///
/// `SOURCE` defaults to the current directory, so `magicfs -s random` needs no
/// `.`; and with no options at all it just reports on the view you're in.
fn root(
    source: Option<PathBuf>,
    args: SpecArgs,
    out: Option<PathBuf>,
    pref: CdPref,
    new: bool,
    ask: Ask,
    dry_run: bool,
) -> Result<()> {
    // Inside a view with no explicit source: this is a reconfiguration, not a
    // request to build a view *of the view*.
    if source.is_none() && out.is_none() && !new
        && let Some(view) = view::current()? {
            if ask != Ask::View {
                return serve(view, args, ask, pref, Standing::Inside, dry_run);
            }
            let root = if args.is_empty() {
                report(&view)?
            } else {
                apply_to_view(view, args, pref, Standing::Inside)?
            };
            // Already standing in it — only an explicit --shell nests another.
            return land(&root, pref, Standing::Inside);
        }

    let view = match (source, view::current()?) {
        (Some(source), _) => open_view(Some(source), out)?,
        // `--new` from inside a view: another view of what *it* presents, not
        // of the view directory itself.
        (None, Some(current)) => sibling_of(&current)?,
        (None, None) => open_view(None, out)?,
    };
    if ask != Ask::View {
        return serve(view, args, ask, pref, Standing::Outside, dry_run);
    }
    let root = apply_to_view(view, args, pref, Standing::Outside)?;
    land(&root, pref, Standing::Outside)
}

/// Build the view the trailing words asked for, then run them in it.
///
/// The two halves of the job pull on each other: which files the command named
/// decides what the view holds, and the view decides what the command is
/// finally handed. So the selection is resolved against a plain scan of the
/// source first, then the view is built, and only then is the argv assembled
/// from the names the view ended up with.
fn serve(
    mut view: View,
    args: SpecArgs,
    ask: Ask,
    pref: CdPref,
    standing: Standing,
    dry_run: bool,
) -> Result<()> {
    args.apply_to(&mut view.spec)?;

    let (words, has_program) = match &ask {
        Ask::Run(words) => (words.clone(), true),
        Ask::Pick(words) => (words.clone(), false),
        Ask::View => unreachable!("serve is only called with something to do"),
    };

    // One quoted argument is a whole command line, and quoting is how you say
    // "leave this alone": hand it to a shell in the view and let *that* expand
    // the glob, which lands in the same place with none of the guesswork.
    let shell_line = has_program && words.len() == 1 && invoke::is_shell_line(&words[0]);

    let invocation = if shell_line {
        None
    } else {
        let entries = scan(&view.source, &view.spec)?;
        let opts = invoke::Options {
            has_program,
            case_sensitive: view.spec.case_sensitive,
            correct: corrector(),
        };
        let picked = invoke::select_with(&words, &view.source, &entries, opts)?;
        // With no command, every word was meant as a file.
        if !has_program && let Some(stray) = picked.argv.first() {
            return Err(not_a_file(stray, &view.source, &entries));
        }
        // Naming no files at all means the whole view, which is also what the
        // view already is — so don't wipe a narrowing an earlier command set.
        if !picked.chosen.is_empty() {
            view.spec.only = picked.chosen.clone();
        }
        Some(picked)
    };

    let plan = build_plan(&view.source, &view.spec)?;
    if plan.is_empty() {
        // Never fall back to "naming nothing means everything" here: with
        // --unseen that would replay every file you have already watched.
        return Err(nothing_new(&view, standing)?);
    }
    links::apply(&view, &plan)?;
    view.save()?;
    eprintln!(
        "{} → {} entries [{}]",
        view.source.display(),
        plan.len(),
        view.spec.summary()
    );
    // A command is about to receive these, and that is what "seen" means.
    // Marked at launch because the command replaces us — there is no "after"
    // to wait for, and a player's exit status says nothing about what you
    // watched anyway. `unsee --last` covers the batch you didn't finish.
    if view.spec.unseen && has_program && !dry_run {
        mark(&view.source, plan.iter().map(|n| &n.entry))?;
    }

    let Some(invocation) = invocation else {
        return launch(&view, shell_argv(&words[0]), pref, standing, dry_run);
    };
    match ask {
        // Files but no command: this was only ever a request for the view.
        Ask::Pick(_) => {
            print_path(&view.root, pref);
            land(&view.root, pref, standing)
        }
        _ => launch(
            &view,
            invocation.with_files(plan.iter().map(|n| n.name.clone())),
            pref,
            standing,
            dry_run,
        ),
    }
}

/// Ask before correcting a typo, tcsh-style — but only when someone is there
/// to answer. A script gets its words exactly as written.
fn corrector() -> Option<invoke::Corrector> {
    let tty = unsafe {
        libc::isatty(libc::STDIN_FILENO) == 1 && libc::isatty(libc::STDERR_FILENO) == 1
    };
    if !tty {
        return None;
    }
    Some(Box::new(|typed: &str, meant: &str| {
        loop {
            eprint!("CORRECT>{meant} (y|n|a)? ");
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line)? == 0 {
                return Ok(invoke::Fix::Abort);
            }
            match line.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" | "" => return Ok(invoke::Fix::Yes),
                "n" | "no" => return Ok(invoke::Fix::No),
                "a" | "abort" | "q" => return Ok(invoke::Fix::Abort),
                _ => eprintln!("  y: use {meant}   n: keep {typed}   a: abort"),
            }
        }
    }))
}

/// "No such file", with the likeliest file named when there is one.
fn not_a_file(word: &str, source: &Path, entries: &[Entry]) -> anyhow::Error {
    match invoke::suggest(word, entries) {
        Some(near) => anyhow::anyhow!(
            "`{word}` is not a file in {} — did you mean `{}`?",
            source.display(),
            near.name
        ),
        None => anyhow::anyhow!("`{word}` is not a file in {}", source.display()),
    }
}

/// Run `argv` inside the view.
///
/// The cd hint goes out before the exec, so a shell with the `shell-init`
/// wrapper installed is left in the view once the command exits — the same
/// place the command itself ran, which is where `cd -` expects to come back
/// from.
fn launch(
    view: &View,
    argv: Vec<String>,
    pref: CdPref,
    standing: Standing,
    dry_run: bool,
) -> Result<()> {
    let (program, rest) = argv.split_first().expect("a command has a program");
    if dry_run {
        // Quoted where it matters, so the printed line is one you could paste.
        let shown: Vec<String> = argv
            .iter()
            .map(|w| {
                if w.contains(char::is_whitespace) {
                    format!("'{w}'")
                } else {
                    w.clone()
                }
            })
            .collect();
        println!("{}", shown.join(" "));
        return Ok(());
    }
    if pref != CdPref::Never && standing == Standing::Outside {
        emit_cd(&view.root)?;
    }
    std::env::set_current_dir(&view.root)
        .with_context(|| format!("cannot enter {}", view.root.display()))?;

    use std::os::unix::process::CommandExt;
    // exec() replaces this process, so the tool inherits the terminal, the
    // signal handling and the exit status directly.
    let err = std::process::Command::new(program)
        .args(rest)
        .env("MAGICFS_VIEW", &view.root)
        .exec();
    Err(err).with_context(|| format!("cannot run `{program}`"))
}

fn shell_argv(line: &str) -> Vec<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    vec![shell, "-c".to_string(), line.to_string()]
}

/// A second, independent view of the same directory, starting from the same
/// ordering — so `--new` compares two variations rather than resetting to
/// defaults and losing the filters you had set up.
fn sibling_of(view: &View) -> Result<View> {
    let mut fresh = open_view(Some(view.source.clone()), None)?;
    fresh.spec = view.spec.clone();
    Ok(fresh)
}

/// Resolve a source directory to the view that presents it, creating the view
/// record if this is the first time we've seen that directory.
///
/// The spec of an existing view is reused, so `magicfs ~/photos` twice doesn't
/// reset the ordering you had set up.
fn open_view(source: Option<PathBuf>, out: Option<PathBuf>) -> Result<View> {
    let requested = source.unwrap_or(std::env::current_dir()?);
    if !requested.exists() {
        bail!(
            "no such directory: {}\n\
             (magicfs works on a directory; `magicfs --help` lists the subcommands)",
            requested.display()
        );
    }
    let source = requested
        .canonicalize()
        .with_context(|| format!("resolving {}", requested.display()))?;
    view::validate_source(&source)?;

    let base = view::base_dir();
    std::fs::create_dir_all(&base).with_context(|| format!("creating {}", base.display()))?;
    let root = view::allocate_root(&source, &base, out)?;

    let spec = match View::load(&root) {
        Ok(existing) => existing.spec,
        Err(_) => ViewSpec { seed: fresh_seed(), ..Default::default() },
    };
    Ok(View { root, source, spec })
}

/// Rebuild a view, persist it, and tell the user (and the shell) where it is.
fn apply_to_view(
    mut view: View,
    args: SpecArgs,
    pref: CdPref,
    standing: Standing,
) -> Result<PathBuf> {
    args.apply_to(&mut view.spec)?;

    let plan = build_plan(&view.source, &view.spec)?;
    // Caught up is an answer, not an empty view to move into.
    if plan.is_empty() && view.spec.unseen {
        return Err(nothing_new(&view, standing)?);
    }
    let stats = links::apply(&view, &plan)?;
    view.save()?;

    eprintln!(
        "{} → {} entries [{}]",
        view.source.display(),
        stats.total(),
        view.spec.summary()
    );
    if stats.total() == 0 {
        eprintln!("  (nothing matched — try `magicfs clear` to drop the filters)");
    }

    // The cd hint is emitted by `land`, not here, so `--no-cd` really does
    // leave the shell alone.
    print_path(&view.root, pref);
    Ok(view.root)
}

/// Put the resulting path on stdout, where that is what it is for.
///
/// At a terminal with the shell about to move there anyway, the path is one
/// more line to read past — the prompt is about to show it. Captured, it is
/// the entire point, so `$(magicfs .)` must always get it.
fn print_path(path: &Path, pref: CdPref) {
    if pref == CdPref::Never || !is_interactive() {
        println!("{}", path.display());
    }
}

/// Apply a spec mutation to the view containing the cwd — or, when we aren't
/// in one, to a fresh view of the current directory.
///
/// That fallback is what makes `cd ~/photos; magicfs shuffle; ls` the whole
/// interaction: reordering a plain directory implies wanting a view of it, so
/// there is nothing to set up first.
fn mutate(pref: CdPref, new: bool, f: impl FnOnce(&mut ViewSpec) -> Result<()>) -> Result<()> {
    let (mut view, standing) = target_view(new)?;
    f(&mut view.spec)?;
    let root = apply_to_view(view, SpecArgs::default(), pref, standing)?;
    land(&root, pref, standing)
}

fn reconfigure(pref: CdPref, new: bool, args: SpecArgs) -> Result<()> {
    let (view, standing) = target_view(new)?;
    let root = apply_to_view(view, args, pref, standing)?;
    land(&root, pref, standing)
}

/// The view a reordering command should act on, and whether the shell is
/// already standing in it.
fn target_view(new: bool) -> Result<(View, Standing)> {
    match view::current()? {
        Some(view) if !new => Ok((view, Standing::Inside)),
        Some(view) => Ok((sibling_of(&view)?, Standing::Outside)),
        None => Ok((open_view(None, None)?, Standing::Outside)),
    }
}

/// Whether the calling shell is already standing in the directory we want it
/// to end up in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Standing {
    Inside,
    Outside,
}

/// Move the user to `dir`, if that is both wanted and possible.
///
/// Three ways to land somewhere, in order of preference: the `shell-init`
/// wrapper cds the real shell (so `cd -` goes back); failing that, at a
/// terminal, a subshell (`exit` goes back); and when output is being captured,
/// nothing at all — `$(magicfs .)` must stay a plain path on stdout.
///
/// Every command that produces a directory the user asked to be in routes
/// through here, so they all behave the same way.
fn land(dir: &Path, pref: CdPref, standing: Standing) -> Result<()> {
    if pref == CdPref::Never {
        return Ok(());
    }
    // Hand the path to the wrapper whether or not we also spawn a shell: it is
    // the wrapper, not us, that decides to act on it.
    if standing == Standing::Outside {
        emit_cd(dir)?;
    }

    let subshell = match pref {
        CdPref::Subshell => true,
        CdPref::Never => unreachable!(),
        // Already there, or the wrapper is about to move us: a subshell would
        // only add a level to unwind.
        CdPref::Auto => {
            standing == Standing::Outside
                && std::env::var_os("MAGICFS_CD_FILE").is_none()
                && is_interactive()
        }
    };

    // Before any exec: the listing has to reach the terminal while we still
    // exist as a process.
    show_listing(dir);

    if subshell { enter_shell(dir) } else { Ok(()) }
}

/// What to run to show the directory we just landed in.
///
/// `-L` matters: without it `ls` reports the size and date of each *symlink*
/// rather than of the file it points at, which reads as nonsense next to
/// `[size desc]`.
const DEFAULT_LS: &str = "ls -lhL --color=auto";

/// Show the contents of the directory we landed in.
///
/// The result *is* the answer to `magicfs -s size` — making the user type `ls`
/// to see it wastes the round trip. Long listings are not truncated: a
/// directory of 5,000 photos scrolls, exactly as `ls` would, and the terminal's
/// scrollback is a better place to solve that than a guess about what counts as
/// too many. Skipped when output is captured, since then this is a
/// path-producing tool and nothing more.
fn show_listing(dir: &Path) {
    if !is_interactive() {
        return;
    }
    let cmd = std::env::var("MAGICFS_LS").unwrap_or_else(|_| DEFAULT_LS.to_string());
    let mut words = cmd.split_whitespace();
    // `MAGICFS_LS=` is how you turn this off.
    let Some(program) = words.next() else { return };

    // Failure here is cosmetic: a missing `ls` must not fail the reorder that
    // actually did the work.
    let _ = std::process::Command::new(program)
        .args(words)
        .current_dir(dir)
        .status();
}

/// True when both stdin and stdout are a terminal — i.e. a person is typing,
/// and nothing is capturing our stdout.
fn is_interactive() -> bool {
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 && libc::isatty(libc::STDOUT_FILENO) == 1 }
}

/// Replace this process with a shell rooted in the view.
///
/// The only way to *natively* put the user in the view: a process cannot
/// change its parent's working directory, so instead of moving the calling
/// shell we start a new one that is already there. Exiting it returns the user
/// exactly where they were, because the original shell never moved.
fn enter_shell(dir: &Path) -> Result<()> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

    // Nothing to announce: the listing above and the prompt below both already
    // show where you are, and MAGICFS_SHELL is in the environment for a prompt
    // that wants to say more.
    std::env::set_current_dir(dir)
        .with_context(|| format!("cannot enter {}", dir.display()))?;

    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(&shell)
        // Lets prompts advertise the view, and lets us spot nesting.
        .env("MAGICFS_SHELL", dir)
        .exec();
    Err(err).with_context(|| format!("cannot start {shell}"))
}

/// What `magicfs close` should act on: the view we're standing in, or — when
/// we're in a plain directory — every view of it.
///
/// The second case is the common one after leaving an auto-opened subshell:
/// you're back in `~/photos`, and since each invocation makes its own view
/// there may be several of them. Closing the lot is the only reading that
/// leaves nothing behind for you to hunt down.
fn views_to_close() -> Result<Vec<View>> {
    if let Some(view) = view::current()? {
        return Ok(vec![view]);
    }
    let cwd = std::env::current_dir()?.canonicalize()?;
    let mine: Vec<View> = view::list_views().into_iter().filter(|v| v.source == cwd).collect();
    if mine.is_empty() {
        bail!("no magicfs view of {}", cwd.display());
    }
    Ok(mine)
}

/// The view we're standing in — for the commands that can only ever mean an
/// existing view (`status`, `which`).
fn current_view() -> Result<View> {
    view::current()?.ok_or_else(|| {
        anyhow::anyhow!(
            "not inside a magicfs view.\n\
             Run `magicfs` (or `magicfs -s random`, `magicfs -s time`, ...) in \
             a directory to open one."
        )
    })
}

/// Where the current command should read its files from: the view we're
/// standing in, or the current directory if we aren't in one.
fn resolve_source(args: &SpecArgs) -> Result<(PathBuf, ViewSpec)> {
    match view::current()? {
        Some(view) => {
            let mut spec = view.spec.clone();
            args.apply_to(&mut spec)?;
            Ok((view.source, spec))
        }
        None => Ok((std::env::current_dir()?, args.to_spec()?)),
    }
}

fn report(view: &View) -> Result<PathBuf> {
    let plan = build_plan(&view.source, &view.spec)?;
    eprintln!("view    {}", view.root.display());
    eprintln!("source  {}", view.source.display());
    eprintln!("order   {}", view.spec.summary());
    eprintln!("entries {}", plan.len());
    if let Some(first) = plan.first() {
        eprintln!("first   {}", first.name);
    }
    if let Some(last) = plan.last().filter(|_| plan.len() > 1) {
        eprintln!("last    {}", last.name);
    }
    println!("{}", view.root.display());
    Ok(view.root.clone())
}

fn status() -> Result<()> {
    report(&current_view()?).map(|_| ())
}

fn list() -> Result<()> {
    let views = view::list_views();
    if views.is_empty() {
        eprintln!("no views. create one with `magicfs <dir>`");
        return Ok(());
    }
    for view in views {
        println!(
            "{}\t{}\t[{}]",
            view.root.display(),
            view.source.display(),
            view.spec.summary()
        );
    }
    Ok(())
}

fn close(all: bool) -> Result<()> {
    let targets = if all { view::list_views() } else { views_to_close()? };
    if targets.is_empty() {
        eprintln!("no views to close");
        return Ok(());
    }

    let cwd = std::env::current_dir().ok();
    for view in targets {
        // Closing the view you are standing in would strand the shell in a
        // deleted directory, so get it back to the real source first.
        let standing_in_it = cwd.as_deref().is_some_and(|c| c.starts_with(&view.root));
        let mut keep_dir = false;
        if standing_in_it {
            emit_cd(&view.source)?;
            let src = view.source.display();
            if std::env::var_os("MAGICFS_CD_FILE").is_some() {
                // The wrapper cds the real shell the moment we exit.
                eprintln!("returning to {src}");
            } else {
                // Nothing will move this shell, so the directory has to outlive
                // the view or every later command dies on getcwd.
                keep_dir = true;
                if std::env::var_os("MAGICFS_SHELL").is_some() {
                    eprintln!("`exit` to return to {src}");
                } else {
                    eprintln!("your shell is still in the closed view — cd {src}");
                }
            }
        }
        links::close(&view, keep_dir)?;
        let _ = view::registry_remove(&view.root);
        eprintln!("closed {}", view.root.display());
    }
    Ok(())
}

/// `magicfs clean` — remove every view, plus the empty directories left behind
/// by views closed while a shell was standing in them.
///
/// Views are nothing but symlinks, so this can never cost anything real; the
/// `--yes` gate exists because it will pull the rug from under any other
/// terminal currently sitting in a view.
fn clean(yes: bool) -> Result<()> {
    let views = view::list_views();
    let stale = view::stale_dirs(&view::base_dir());
    if views.is_empty() && stale.is_empty() {
        eprintln!("nothing to clean");
        return Ok(());
    }

    if !yes {
        for view in &views {
            eprintln!("would close {}  ({})", view.root.display(), view.source.display());
        }
        for dir in &stale {
            eprintln!("would remove {}  (leftover)", dir.display());
        }
        eprintln!(
            "\n{} view(s), {} leftover(s) — links only, no real files. Re-run with --yes.",
            views.len(),
            stale.len()
        );
        return Ok(());
    }

    let cwd = std::env::current_dir().ok();
    let mut removed = 0usize;
    for view in views {
        // Same rule as `close`: never delete the directory this shell is in.
        let standing_in_it = cwd.as_deref().is_some_and(|c| c.starts_with(&view.root));
        if standing_in_it {
            emit_cd(&view.source)?;
            eprintln!("you are in {} — cd out, or `exit`", view.root.display());
        }
        links::close(&view, standing_in_it)?;
        let _ = view::registry_remove(&view.root);
        removed += 1;
    }
    for dir in stale {
        if cwd.as_deref().is_some_and(|c| c.starts_with(&dir)) {
            continue;
        }
        if std::fs::remove_dir(&dir).is_ok() {
            removed += 1;
        }
    }
    eprintln!("cleaned {removed} directories");
    Ok(())
}

/// Run a command with the ordered files as arguments.
///
/// This is the escape hatch from index prefixes: the tool receives the real
/// paths, with their original names, in our order — because argv order is one
/// of the few orderings nothing downstream re-sorts.
fn exec(args: SpecArgs, dry_run: bool, command: &[String]) -> Result<()> {
    let (source, spec) = resolve_source(&args)?;
    let plan = build_plan(&source, &spec)?;

    if plan.is_empty() {
        if spec.unseen {
            return Err(caught_up(&source, &spec)?);
        }
        bail!("nothing matched [{}] in {}", spec.summary(), source.display());
    }
    if spec.unseen && !dry_run {
        mark(&source, plan.iter().map(|n| &n.entry))?;
    }

    let (program, leading) = command.split_first().expect("clap requires a command");
    let files: Vec<&Path> = plan.iter().map(|n| n.entry.path.as_path()).collect();

    if dry_run {
        let mut out = String::from(program.as_str());
        for arg in leading {
            out.push(' ');
            out.push_str(arg);
        }
        for f in &files {
            out.push(' ');
            out.push_str(&f.to_string_lossy());
        }
        println!("{out}");
        return Ok(());
    }

    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(program);
    cmd.args(leading).args(&files);

    // exec() replaces this process, so the tool inherits the terminal and
    // signal handling directly instead of running under a wrapper.
    let err = cmd.exec();
    Err(err).with_context(|| format!("cannot run `{program}`"))
}

fn paths(args: SpecArgs, print0: bool) -> Result<()> {
    let (source, spec) = resolve_source(&args)?;
    let plan = build_plan(&source, &spec)?;

    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    for named in &plan {
        out.write_all(named.entry.path.as_os_str().as_encoded_bytes())?;
        out.write_all(if print0 { b"\0" } else { b"\n" })?;
    }
    out.flush()?;
    Ok(())
}

/// `magicfs demo` — build a sample directory, drop the user into it, and
/// suggest what to try.
///
/// It lands you in the sample directory itself rather than a view of it: the
/// point of the demo is to run magicfs *on* something and watch the order
/// change, which needs a before as well as an after.
///
/// The suggestions are deliberately free of shell-specific syntax: `$(...)` is
/// a bash-ism that tcsh rejects outright, and a first-run hint that errors is
/// worse than no hint at all.
fn demo(count: usize, out: Option<PathBuf>, pref: CdPref) -> Result<()> {
    if count == 0 {
        bail!("--count must be at least 1");
    }
    let dir = crate::demo::create(out, count)?;

    let spec = ViewSpec { dirs: crate::spec::DirMode::Exclude, ..Default::default() };
    let files = build_plan(&dir, &spec)?;
    eprintln!(
        "created {} files in {} — try `magicfs -s size`, then `ls`",
        files.len(),
        dir.display()
    );

    print_path(&dir, pref);
    land(&dir, pref, Standing::Outside)
}

fn which(name: &str) -> Result<()> {
    let view = current_view()?;
    let link = view.root.join(name);
    let target = std::fs::read_link(&link)
        .with_context(|| format!("{name} is not an entry in {}", view.root.display()))?;
    println!("{}", target.display());
    Ok(())
}

/// Record entries as seen and say so, along with how to take it back.
fn mark<'a>(source: &Path, entries: impl IntoIterator<Item = &'a Entry>) -> Result<()> {
    let mut seen = Seen::load(source)?;
    let added = seen.mark(entries);
    seen.save()?;
    if added > 0 {
        eprintln!("marked {added} seen — `magicfs unsee --last` puts them back");
    }
    Ok(())
}

/// The error for an empty result: "you're caught up" when --unseen is what
/// emptied it, the usual "nothing matched" otherwise.
///
/// A view allocated just for this invocation is removed again — there is
/// nothing in it to move into, and no reason to leave a husk for `clean`.
fn nothing_new(view: &View, standing: Standing) -> Result<anyhow::Error> {
    if standing == Standing::Outside {
        let _ = std::fs::remove_dir(&view.root);
    }
    caught_up(&view.source, &view.spec)
}

fn caught_up(source: &Path, spec: &ViewSpec) -> Result<anyhow::Error> {
    // What the view would hold if nothing had been seen: the files --unseen
    // is actually hiding, so the count means something.
    let everything = ViewSpec { unseen: false, limit: None, ..spec.clone() };
    let entries = arrange(scan(source, &everything)?, &everything)?;
    let seen = Seen::load(source)?;
    let hidden = if spec.unseen { seen.count_in(&entries) } else { 0 };
    if hidden == 0 {
        return Ok(anyhow::anyhow!(
            "nothing matched [{}] in {}",
            spec.summary(),
            source.display()
        ));
    }
    let when = seen.last_marked_ago().map(|s| format!(", last marked {}", seen::ago(s)));
    Ok(anyhow::anyhow!(
        "nothing new in {} ({hidden} seen{})",
        source.display(),
        when.unwrap_or_default()
    ))
}

/// The directory `seen`/`unsee` act on, and everything in it — the view's
/// source when standing in one, so view names like `001-a.mp4` resolve too.
fn seen_scope() -> Result<(PathBuf, Vec<Entry>)> {
    let (source, spec) = resolve_source(&SpecArgs::default())?;
    let entries = scan(&source, &spec)?;
    Ok((source, entries))
}

/// The entries these words name. Every word has to name one: a typo in
/// `magicfs unsee` must not quietly do nothing.
fn named<'a>(files: &[String], source: &Path, entries: &'a [Entry]) -> Result<Vec<&'a Entry>> {
    let opts = invoke::Options { correct: corrector(), ..Default::default() };
    let picked = invoke::select_with(files, source, entries, opts)?;
    if let Some(stray) = picked.argv.first() {
        return Err(not_a_file(stray, source, entries));
    }
    // `select` reports naming every file as naming none in particular.
    if picked.chosen.is_empty() {
        return Ok(entries.iter().collect());
    }
    let chosen: std::collections::HashSet<&str> =
        picked.chosen.iter().map(String::as_str).collect();
    Ok(entries.iter().filter(|e| chosen.contains(e.rel.as_str())).collect())
}

/// `magicfs seen [FILES...]` — mark by hand, or report.
///
/// `magicfs seen *` is how you start: everything already there counts as
/// watched, and the next `--unseen` shows only what arrives after.
fn mark_seen(files: &[String]) -> Result<()> {
    let (source, entries) = seen_scope()?;
    let mut seen = Seen::load(&source)?;
    if files.is_empty() {
        let when = seen
            .last_marked_ago()
            .map(|s| format!(", last marked {} ({} files)", seen::ago(s), seen.last_batch_len()))
            .unwrap_or_default();
        eprintln!(
            "{}: {} of {} seen{when}",
            source.display(),
            seen.count_in(&entries),
            entries.len()
        );
        return Ok(());
    }
    let added = seen.mark(named(files, &source, &entries)?);
    seen.save()?;
    eprintln!("marked {added} seen in {}", source.display());
    Ok(())
}

/// `magicfs unsee --last | --all | FILES...`
fn unsee(last: bool, all: bool, files: &[String]) -> Result<()> {
    let (source, entries) = seen_scope()?;
    let mut seen = Seen::load(&source)?;
    let removed = if last {
        if !files.is_empty() {
            bail!("--last undoes a whole batch; drop the file names, or drop --last");
        }
        seen.unmark_last()
    } else if all {
        seen.clear()
    } else if files.is_empty() {
        bail!("unsee what? name files, or pass --last (the latest batch) or --all");
    } else {
        seen.unmark(named(files, &source, &entries)?)
    };
    seen.save()?;
    eprintln!("{removed} unseen again in {}", source.display());
    Ok(())
}

/// Hand the view path to the shell wrapper installed by `magicfs shell-init`,
/// which cds into it. Silently does nothing when no wrapper is active.
fn emit_cd(path: &Path) -> Result<()> {
    if let Some(file) = std::env::var_os("MAGICFS_CD_FILE") {
        std::fs::write(&file, path.as_os_str().as_encoded_bytes())
            .with_context(|| format!("writing cd hint to {}", PathBuf::from(&file).display()))?;
    }
    Ok(())
}
