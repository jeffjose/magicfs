//! Command implementations.
//!
//! Output convention: the view's path is the only thing written to stdout, so
//! `cd "$(magicfs ~/photos -s time)"` works. Everything else goes to stderr.

use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::cli::{Cli, Command, ShellPref, SpecArgs};
use crate::entry::scan;
use crate::links;
use crate::naming::{Named, render};
use crate::order::arrange;
use crate::spec::{ViewSpec, fresh_seed};
use crate::view::{self, View};

pub fn run(cli: Cli) -> Result<()> {
    let pref = cli.shell_pref();
    match cli.command {
        None => root(cli.source, cli.spec, cli.out, pref),
        Some(Command::Sort { key, reverse }) => reconfigure(
            pref,
            SpecArgs { sort: Some(key), reverse, ..Default::default() },
        ),
        Some(Command::Reverse) => mutate(pref, |spec| {
            spec.reverse = !spec.reverse;
            Ok(())
        }),
        Some(Command::Filter { patterns }) => mutate(pref, move |spec| {
            spec.filter = patterns.clone();
            Ok(())
        }),
        Some(Command::Exclude { patterns }) => mutate(pref, move |spec| {
            spec.exclude = patterns.clone();
            Ok(())
        }),
        Some(Command::Limit { n }) => mutate(pref, move |spec| {
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
        Some(Command::Clear) => mutate(pref, |spec| {
            spec.filter.clear();
            spec.exclude.clear();
            spec.limit = None;
            Ok(())
        }),
        Some(Command::Refresh) => mutate(pref, |_| Ok(())),
        Some(Command::Status) => status(),
        Some(Command::List) => list(),
        Some(Command::Close { all }) => close(all),
        Some(Command::Exec { spec, dry_run, command }) => exec(spec, dry_run, &command),
        Some(Command::Paths { spec, print0 }) => paths(spec, print0),
        Some(Command::Which { name }) => which(&name),
        Some(Command::Demo { count, out, open }) => demo(count, out, open, pref),
        Some(Command::ShellInit { name }) => {
            print!("{}", crate::shellinit::script(name.as_deref())?);
            Ok(())
        }
    }
}

/// Resolve a spec into an ordered, named plan.
pub fn build_plan(source: &Path, spec: &ViewSpec) -> Result<Vec<Named>> {
    let entries = scan(source, spec)?;
    Ok(render(arrange(entries, spec)?, spec))
}

/// `magicfs [SOURCE] [OPTIONS]` — create a view, or reconfigure the one we're
/// standing in.
///
/// `SOURCE` defaults to the current directory, so `magicfs -s random` needs no
/// `.`; and with no options at all it just reports on the view you're in.
fn root(
    source: Option<PathBuf>,
    args: SpecArgs,
    out: Option<PathBuf>,
    pref: ShellPref,
) -> Result<()> {
    // Inside a view with no explicit source: this is a reconfiguration, not a
    // request to build a view *of the view*.
    if source.is_none() && out.is_none()
        && let Some(view) = view::current()? {
            let root = if args.is_empty() {
                report(&view)?
            } else {
                apply_to_view(view, args)?
            };
            // Already standing in it — only an explicit --shell nests another.
            return maybe_enter(&root, pref, Standing::Inside);
        }

    let view = open_view(source, out)?;
    let root = apply_to_view(view, args)?;
    maybe_enter(&root, pref, Standing::Outside)
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
fn apply_to_view(mut view: View, args: SpecArgs) -> Result<PathBuf> {
    args.apply_to(&mut view.spec)?;

    let plan = build_plan(&view.source, &view.spec)?;
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

    emit_cd(&view.root)?;
    println!("{}", view.root.display());
    Ok(view.root)
}

/// Apply a spec mutation to the view containing the cwd — or, when we aren't
/// in one, to a fresh view of the current directory.
///
/// That fallback is what makes `cd ~/photos; magicfs shuffle; ls` the whole
/// interaction: reordering a plain directory implies wanting a view of it, so
/// there is nothing to set up first.
fn mutate(pref: ShellPref, f: impl FnOnce(&mut ViewSpec) -> Result<()>) -> Result<()> {
    let (mut view, standing) = match view::current()? {
        Some(view) => (view, Standing::Inside),
        None => (open_view(None, None)?, Standing::Outside),
    };
    f(&mut view.spec)?;
    let root = apply_to_view(view, SpecArgs::default())?;
    maybe_enter(&root, pref, standing)
}

fn reconfigure(pref: ShellPref, args: SpecArgs) -> Result<()> {
    let (view, standing) = match view::current()? {
        Some(view) => (view, Standing::Inside),
        None => (open_view(None, None)?, Standing::Outside),
    };
    let root = apply_to_view(view, args)?;
    maybe_enter(&root, pref, standing)
}

/// Whether the calling shell was already inside the view we just touched.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Standing {
    Inside,
    Outside,
}

/// Put the user in the view, if that is both wanted and useful.
///
/// Three ways to land somewhere, in order of preference: the `shell-init`
/// wrapper cds the real shell (so `cd -` goes back); failing that, at a
/// terminal, a subshell (`exit` goes back); and when output is being captured,
/// nothing at all — `$(magicfs .)` must stay a plain path on stdout.
fn maybe_enter(root: &Path, pref: ShellPref, standing: Standing) -> Result<()> {
    let enter = match pref {
        ShellPref::Always => true,
        ShellPref::Never => false,
        // Already there, or the wrapper is about to move us: a subshell would
        // only add a level to unwind.
        ShellPref::Auto => {
            standing == Standing::Outside
                && std::env::var_os("MAGICFS_CD_FILE").is_none()
                && is_interactive()
        }
    };
    if enter { enter_shell(root, pref) } else { Ok(()) }
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
fn enter_shell(dir: &Path, pref: ShellPref) -> Result<()> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let nested = std::env::var_os("MAGICFS_SHELL").is_some();

    if nested {
        eprintln!("note: already in a magicfs shell — `exit` unwinds one level");
    }
    eprintln!("entering {} — `exit` to leave", dir.display());
    // Only nag the first time, and only when we chose the subshell ourselves:
    // someone who typed --shell already knows what they asked for.
    if pref == ShellPref::Auto && !nested && let Ok(name) = crate::shellinit::detect() {
        eprintln!("(to cd in place instead, install the wrapper: magicfs shell-init {name})");
    }

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
/// we're in a plain directory — the view that presents it.
///
/// The second case is the common one after leaving an auto-opened subshell:
/// you're back in `~/photos` and the view still exists, so `magicfs close`
/// there has exactly one sensible meaning.
fn view_to_close() -> Result<View> {
    if let Some(view) = view::current()? {
        return Ok(view);
    }
    let cwd = std::env::current_dir()?.canonicalize()?;
    view::list_views()
        .into_iter()
        .find(|v| v.source == cwd)
        .ok_or_else(|| anyhow::anyhow!("no magicfs view of {}", cwd.display()))
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
    let targets = if all { view::list_views() } else { vec![view_to_close()?] };
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

/// Run a command with the ordered files as arguments.
///
/// This is the escape hatch from index prefixes: the tool receives the real
/// paths, with their original names, in our order — because argv order is one
/// of the few orderings nothing downstream re-sorts.
fn exec(args: SpecArgs, dry_run: bool, command: &[String]) -> Result<()> {
    let (source, spec) = resolve_source(&args)?;
    let plan = build_plan(&source, &spec)?;

    if plan.is_empty() {
        bail!("nothing matched [{}] in {}", spec.summary(), source.display());
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

/// `magicfs demo` — build a sample directory and suggest what to try in it.
///
/// The suggestions are deliberately free of shell-specific syntax: `$(...)` is
/// a bash-ism that tcsh rejects outright, and a first-run hint that errors is
/// worse than no hint at all.
fn demo(count: usize, out: Option<PathBuf>, open: bool, pref: ShellPref) -> Result<()> {
    if count == 0 {
        bail!("--count must be at least 1");
    }
    let dir = crate::demo::create(out, count)?;

    let spec = ViewSpec { dirs: crate::spec::DirMode::Exclude, ..Default::default() };
    let files = build_plan(&dir, &spec)?;
    eprintln!("created {} files in {}", files.len(), dir.display());
    eprintln!();
    eprintln!("  cd {}", dir.display());
    eprintln!("  magicfs -s size      # opens a view, biggest first");
    eprintln!("  ls");
    eprintln!("  magicfs -s random    # `feh *` now opens in random order");
    eprintln!("  magicfs -s natural");
    eprintln!("  magicfs filter images");
    eprintln!("  magicfs close        # removes the view, not the files");

    // `demo --shell` reads as "put me in it", so treat it as `--open` too.
    if open || pref == ShellPref::Always {
        let view = open_view(Some(dir), None)?;
        let root = apply_to_view(view, SpecArgs::default())?;
        return maybe_enter(&root, pref, Standing::Outside);
    }
    println!("{}", dir.display());
    Ok(())
}

fn which(name: &str) -> Result<()> {
    let view = current_view()?;
    let link = view.root.join(name);
    let target = std::fs::read_link(&link)
        .with_context(|| format!("{name} is not an entry in {}", view.root.display()))?;
    println!("{}", target.display());
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
