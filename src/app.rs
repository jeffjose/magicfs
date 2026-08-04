//! Command implementations.
//!
//! Output convention: the view's path is the only thing written to stdout, so
//! `cd "$(magicfs ~/photos -s time)"` works. Everything else goes to stderr.

use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::cli::{Cli, Command, SpecArgs};
use crate::entry::scan;
use crate::links;
use crate::naming::{Named, render};
use crate::order::arrange;
use crate::spec::{SortKey, ViewSpec, fresh_seed};
use crate::view::{self, View};

pub fn run(cli: Cli) -> Result<()> {
    match cli.command {
        None => root(cli.source, cli.spec, cli.out),
        Some(Command::Sort { key, reverse }) => reconfigure(SpecArgs {
            sort: Some(key),
            reverse,
            ..Default::default()
        }),
        Some(Command::Shuffle) => mutate(|spec| {
            spec.sort = SortKey::Random;
            spec.reverse = false;
            spec.seed = fresh_seed();
            Ok(())
        }),
        Some(Command::Reverse) => mutate(|spec| {
            spec.reverse = !spec.reverse;
            Ok(())
        }),
        Some(Command::Filter { patterns }) => mutate(move |spec| {
            spec.filter = patterns.clone();
            Ok(())
        }),
        Some(Command::Exclude { patterns }) => mutate(move |spec| {
            spec.exclude = patterns.clone();
            Ok(())
        }),
        Some(Command::Limit { n }) => mutate(move |spec| {
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
        Some(Command::Clear) => mutate(|spec| {
            spec.filter.clear();
            spec.exclude.clear();
            spec.limit = None;
            Ok(())
        }),
        Some(Command::Refresh) => mutate(|_| Ok(())),
        Some(Command::Status) => status(),
        Some(Command::List) => list(),
        Some(Command::Close { all }) => close(all),
        Some(Command::Exec { spec, dry_run, command }) => exec(spec, dry_run, &command),
        Some(Command::Paths { spec, print0 }) => paths(spec, print0),
        Some(Command::Which { name }) => which(&name),
        Some(Command::ShellInit { shell }) => {
            print!("{}", crate::shellinit::script(shell.as_deref())?);
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
fn root(source: Option<PathBuf>, args: SpecArgs, out: Option<PathBuf>) -> Result<()> {
    // Inside a view with no explicit source: this is a reconfiguration, not a
    // request to build a view *of the view*.
    if source.is_none() && out.is_none()
        && let Some(view) = view::current()? {
            return if args.is_empty() {
                report(&view)
            } else {
                apply_to_view(view, args)
            };
        }

    let source = source.unwrap_or(std::env::current_dir()?);
    let source = source
        .canonicalize()
        .with_context(|| format!("no such directory: {}", source.display()))?;
    view::validate_source(&source)?;

    let base = view::base_dir();
    std::fs::create_dir_all(&base)
        .with_context(|| format!("creating {}", base.display()))?;
    let root = view::allocate_root(&source, &base, out)?;

    // Reuse the existing spec when re-entering a view we already built, so
    // `magicfs ~/photos` twice doesn't reset the ordering you had set up.
    let mut spec = match View::load(&root) {
        Ok(existing) => existing.spec,
        Err(_) => ViewSpec { seed: fresh_seed(), ..Default::default() },
    };
    args.apply_to(&mut spec)?;

    apply_to_view(View { root, source, spec }, SpecArgs::default())
}

/// Rebuild a view, persist it, and tell the user (and the shell) where it is.
fn apply_to_view(mut view: View, args: SpecArgs) -> Result<()> {
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
    Ok(())
}

/// Apply a spec mutation to the view containing the cwd.
fn mutate(f: impl FnOnce(&mut ViewSpec) -> Result<()>) -> Result<()> {
    let mut view = current_view()?;
    f(&mut view.spec)?;
    apply_to_view(view, SpecArgs::default())
}

fn reconfigure(args: SpecArgs) -> Result<()> {
    let view = current_view()?;
    apply_to_view(view, args)
}

fn current_view() -> Result<View> {
    view::current()?.ok_or_else(|| {
        anyhow::anyhow!(
            "not inside a magicfs view.\n\
             Create one first:  cd \"$(magicfs .)\""
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

fn report(view: &View) -> Result<()> {
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
    Ok(())
}

fn status() -> Result<()> {
    report(&current_view()?)
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
    let targets = if all {
        view::list_views()
    } else {
        vec![current_view()?]
    };
    if targets.is_empty() {
        eprintln!("no views to close");
        return Ok(());
    }

    let cwd = std::env::current_dir().ok();
    for view in targets {
        // Closing the view you are standing in would strand the shell in a
        // deleted directory, so hand it back to the real source.
        if cwd.as_deref().is_some_and(|c| c.starts_with(&view.root)) {
            emit_cd(&view.source)?;
            eprintln!("returning to {}", view.source.display());
        }
        links::close(&view)?;
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
