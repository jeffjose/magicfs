//! `cp`, done here rather than handed to `cp`.
//!
//! Copying out of a view is the one file operation that is routinely slow —
//! the source is as often as not a directory on another machine — and `cp`
//! says nothing until it is finished. So the plain cases are copied by magicfs
//! itself, which knows the whole list up front and can show how far along it
//! is. A command line that asks for anything beyond those cases still goes to
//! the real `cp`.

use anyhow::{Context, Result, bail};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A `cp` command line we can carry out ourselves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    pub dest: PathBuf,
    /// `-n`: leave existing files alone.
    pub no_clobber: bool,
    /// `-i`: ask before replacing one.
    pub interactive: bool,
    /// `-v`: name each file as it lands.
    pub verbose: bool,
}

/// Read `words` — a `cp` command line with the files to copy taken out — as a
/// job, or `None` when it wants something only `cp` knows how to do.
///
/// Times and mode are always kept and directories always descended into, so
/// `-p`, `-r` and `-a` are accepted and change nothing.
pub fn job(words: &[String]) -> Option<Job> {
    let (program, rest) = words.split_first()?;
    if Path::new(program).file_name()?.to_str()? != "cp" {
        return None;
    }
    let mut job = Job {
        dest: PathBuf::new(),
        no_clobber: false,
        interactive: false,
        verbose: false,
    };
    let mut dest = None;
    for word in rest {
        if let Some(long) = word.strip_prefix("--") {
            match long {
                "recursive" | "archive" | "preserve" | "force" => {}
                "verbose" => job.verbose = true,
                "no-clobber" => job.no_clobber = true,
                "interactive" => job.interactive = true,
                _ => return None,
            }
        } else if word.len() > 1 && word.starts_with('-') {
            for flag in word[1..].chars() {
                match flag {
                    'r' | 'R' | 'a' | 'p' | 'f' => {}
                    'v' => job.verbose = true,
                    'n' => job.no_clobber = true,
                    'i' => job.interactive = true,
                    _ => return None,
                }
            }
        } else if dest.replace(PathBuf::from(word)).is_some() {
            // Two words that are not files of ours: not a shape we know.
            return None;
        }
    }
    job.dest = dest?;
    Some(job)
}

enum Kind {
    File,
    Dir,
    Link,
}

/// One thing to create at the destination.
struct Item {
    from: PathBuf,
    to: PathBuf,
    kind: Kind,
    size: u64,
}

impl Job {
    /// Copy `sources` to the destination and return the exit status.
    ///
    /// Like `cp`, one file that can't be copied is reported and the rest
    /// carry on.
    pub fn run(&self, sources: &[PathBuf]) -> Result<i32> {
        let into_dir = self.dest.is_dir();
        if !into_dir && (sources.len() > 1 || self.dest.as_os_str().as_encoded_bytes().ends_with(b"/")) {
            bail!("`{}` is not a directory", self.dest.display());
        }
        let mut failed = 0;
        let mut items = Vec::new();
        for source in sources {
            let to = match source.file_name() {
                Some(name) if into_dir => self.dest.join(name),
                _ => self.dest.clone(),
            };
            if let Err(err) = expand(source, &to, &mut items) {
                eprintln!("magicfs: {err:#}");
                failed += 1;
            }
        }

        let files = items.iter().filter(|i| !matches!(i.kind, Kind::Dir)).count();
        let mut progress = Progress::new(files, items.iter().map(|i| i.size).sum());
        let mut dirs = Vec::new();
        for item in &items {
            progress.on(&item.from);
            let done = match item.kind {
                Kind::Dir => {
                    dirs.push(item);
                    fs::create_dir_all(&item.to).map(|_| true).map_err(Into::into)
                }
                Kind::Link => self.link(item),
                Kind::File => self.file(item, &mut progress),
            };
            match done {
                Ok(copied) => {
                    if !matches!(item.kind, Kind::Dir) {
                        progress.finished(item, copied, self.verbose);
                    }
                }
                Err(err) => {
                    progress.say(&format!("magicfs: cannot copy {}: {err:#}", item.from.display()));
                    progress.skip(item);
                    failed += 1;
                }
            }
        }
        // Last, and deepest first: filling a directory is what moves its time.
        for item in dirs.iter().rev() {
            if let Ok(md) = fs::metadata(&item.from) {
                let _ = stamp(&item.to, &md);
            }
        }
        progress.done(&self.dest, failed);
        Ok(if failed == 0 { 0 } else { 1 })
    }

    /// Whether to write `to`, which may already be there.
    fn wanted(&self, to: &Path, progress: &mut Progress) -> Result<bool> {
        if fs::symlink_metadata(to).is_err() {
            return Ok(true);
        }
        if self.no_clobber {
            return Ok(false);
        }
        if self.interactive {
            progress.clear();
            eprint!("overwrite {}? [y/N] ", to.display());
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            return Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"));
        }
        Ok(true)
    }

    fn file(&self, item: &Item, progress: &mut Progress) -> Result<bool> {
        let md = fs::metadata(&item.from)?;
        if let Ok(there) = fs::metadata(&item.to) {
            // Opening it for writing would empty the very file being read.
            if there.dev() == md.dev() && there.ino() == md.ino() {
                bail!("it and {} are the same file", item.to.display());
            }
            if there.is_dir() {
                bail!("{} is a directory", item.to.display());
            }
        }
        if !self.wanted(&item.to, progress)? {
            return Ok(false);
        }
        let mut src = File::open(&item.from)?;
        let mut dst = File::create(&item.to)
            .with_context(|| format!("cannot write {}", item.to.display()))?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = match src.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            dst.write_all(&buf[..n])?;
            progress.advance(n as u64);
        }
        drop(dst);
        stamp(&item.to, &md)?;
        Ok(true)
    }

    fn link(&self, item: &Item) -> Result<bool> {
        if fs::symlink_metadata(&item.to).is_ok() {
            if self.no_clobber {
                return Ok(false);
            }
            fs::remove_file(&item.to)?;
        }
        std::os::unix::fs::symlink(fs::read_link(&item.from)?, &item.to)?;
        Ok(true)
    }
}

/// Give the copy its original's mode and times.
fn stamp(to: &Path, md: &fs::Metadata) -> Result<()> {
    let times = fs::FileTimes::new().set_accessed(md.accessed()?).set_modified(md.modified()?);
    // Times first: the mode may be one that stops us opening it again.
    File::open(to)?.set_times(times)?;
    fs::set_permissions(to, fs::Permissions::from_mode(md.mode() & 0o7777))?;
    Ok(())
}

/// List what copying `source` to `to` will create: itself, and for a
/// directory everything under it.
fn expand(source: &Path, to: &Path, items: &mut Vec<Item>) -> Result<()> {
    // Followed, as `cp` follows what it is given by name: an entry that is a
    // link to a file is copied as the file.
    let md = fs::metadata(source).with_context(|| format!("cannot read {}", source.display()))?;
    if !md.is_dir() {
        items.push(Item { from: source.to_path_buf(), to: to.to_path_buf(), kind: Kind::File, size: md.len() });
        return Ok(());
    }
    if let (Ok(real), Ok(parent)) = (
        fs::canonicalize(source),
        fs::canonicalize(to.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."))),
    ) && parent.starts_with(&real)
    {
        bail!("cannot copy {} into itself", source.display());
    }
    for dirent in walkdir::WalkDir::new(source).follow_links(false) {
        let dirent = dirent.with_context(|| format!("cannot read {}", source.display()))?;
        let rel = dirent.path().strip_prefix(source).unwrap_or(Path::new(""));
        let to = if rel.as_os_str().is_empty() { to.to_path_buf() } else { to.join(rel) };
        let kind = dirent.file_type();
        // Depth 0 is the directory itself, even when reached through a link.
        let (kind, size) = if kind.is_dir() || dirent.depth() == 0 {
            (Kind::Dir, 0)
        } else if kind.is_symlink() {
            (Kind::Link, 0)
        } else if kind.is_file() {
            (Kind::File, dirent.metadata().map(|m| m.len()).unwrap_or(0))
        } else {
            // A socket or a device: nothing a copy should try to read.
            continue;
        };
        items.push(Item { from: dirent.into_path(), to, kind, size });
    }
    Ok(())
}

/// The line that says how far along the copy is, in the shape cargo uses:
///
/// ```text
///      Copying [=========>               ] 12/40 files, 1.2/3.4 GiB, 11 MiB/s: IMG_2934.jpg
/// ```
///
/// Drawn only at a terminal; a log gets the closing summary alone.
struct Progress {
    files: usize,
    files_done: usize,
    bytes: u64,
    bytes_done: u64,
    copied: usize,
    copied_bytes: u64,
    /// How much of the file in hand has been counted, so a failure part-way
    /// can still move the total past it.
    in_file: u64,
    name: String,
    start: Instant,
    drawn: Option<Instant>,
    tty: bool,
    color: bool,
}

const BAR: usize = 25;

impl Progress {
    fn new(files: usize, bytes: u64) -> Progress {
        let tty = unsafe { libc::isatty(libc::STDERR_FILENO) == 1 };
        Progress {
            files,
            files_done: 0,
            bytes,
            bytes_done: 0,
            copied: 0,
            copied_bytes: 0,
            in_file: 0,
            name: String::new(),
            start: Instant::now(),
            drawn: None,
            tty,
            color: tty && std::env::var_os("NO_COLOR").is_none(),
        }
    }

    fn on(&mut self, from: &Path) {
        self.in_file = 0;
        self.name = from.file_name().unwrap_or(from.as_os_str()).to_string_lossy().into_owned();
        self.draw(false);
    }

    fn advance(&mut self, n: u64) {
        self.in_file += n;
        self.bytes_done += n;
        self.draw(false);
    }

    fn finished(&mut self, item: &Item, copied: bool, verbose: bool) {
        if copied {
            self.copied += 1;
            self.copied_bytes += self.in_file;
            if verbose {
                self.say(&format!("{} -> {}", item.from.display(), item.to.display()));
            }
        }
        self.skip(item);
    }

    /// Count `item` as dealt with, whatever became of it.
    fn skip(&mut self, item: &Item) {
        if !matches!(item.kind, Kind::Dir) {
            self.files_done += 1;
        }
        self.bytes_done += item.size.saturating_sub(self.in_file);
        self.in_file = 0;
    }

    /// Print a line of its own, above the bar.
    fn say(&mut self, line: &str) {
        self.clear();
        eprintln!("{line}");
        self.draw(true);
    }

    fn clear(&mut self) {
        if self.tty && self.drawn.take().is_some() {
            eprint!("\r\x1b[K");
        }
    }

    fn verb(&self, verb: &str) -> String {
        if self.color { format!("\x1b[1;32m{verb:>12}\x1b[0m") } else { format!("{verb:>12}") }
    }

    fn draw(&mut self, force: bool) {
        if !self.tty {
            return;
        }
        let now = Instant::now();
        if !force && self.drawn.is_some_and(|at| now - at < Duration::from_millis(80)) {
            return;
        }
        self.drawn = Some(now);
        let fraction = if self.bytes > 0 {
            self.bytes_done as f64 / self.bytes as f64
        } else {
            self.files_done as f64 / self.files.max(1) as f64
        };
        let filled = ((fraction * BAR as f64) as usize).min(BAR);
        let bar = if filled == BAR {
            "=".repeat(BAR)
        } else {
            format!("{}>{}", "=".repeat(filled), " ".repeat(BAR - filled - 1))
        };
        let secs = self.start.elapsed().as_secs_f64();
        let rate = if secs > 0.5 { format!(", {}/s", human(self.bytes_done as f64 / secs)) } else { String::new() };
        let text = format!(
            " [{bar}] {}/{} files, {}{rate}: {}",
            self.files_done,
            self.files,
            ratio(self.bytes_done, self.bytes),
            self.name
        );
        // One line, never wrapped: a wrapped line can't be drawn over.
        let room = width().saturating_sub(13);
        let text: String = text.chars().take(room).collect();
        eprint!("\r{}{text}\x1b[K", self.verb("Copying"));
        let _ = std::io::stderr().flush();
    }

    fn done(&mut self, dest: &Path, failed: usize) {
        self.clear();
        let noun = if self.copied == 1 { "file" } else { "files" };
        let mut line = format!(
            "{} {} {noun} ({}) to {} in {}",
            self.verb("Copied"),
            self.copied,
            human(self.copied_bytes as f64),
            dest.display(),
            elapsed(self.start.elapsed()),
        );
        let skipped = self.files.saturating_sub(self.copied + failed);
        if skipped > 0 {
            line.push_str(&format!(", {skipped} already there"));
        }
        if failed > 0 {
            line.push_str(&format!(", {failed} failed"));
        }
        eprintln!("{line}");
    }
}

fn width() -> usize {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut size) == 0 };
    if ok && size.ws_col > 0 { size.ws_col as usize } else { 80 }
}

const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

fn scaled(bytes: f64, unit: usize) -> String {
    let value = bytes / 1024f64.powi(unit as i32);
    if unit == 0 || value >= 100.0 { format!("{value:.0}") } else { format!("{value:.1}") }
}

fn unit_for(bytes: f64) -> usize {
    let mut unit = 0;
    while unit + 1 < UNITS.len() && bytes >= 1024f64.powi(unit as i32 + 1) {
        unit += 1;
    }
    unit
}

/// `3.4 GiB`.
fn human(bytes: f64) -> String {
    let unit = unit_for(bytes);
    format!("{} {}", scaled(bytes, unit), UNITS[unit])
}

/// `1.2/3.4 GiB` — both in the total's unit, so the pair reads as a fraction.
fn ratio(done: u64, total: u64) -> String {
    let unit = unit_for(total as f64);
    format!("{}/{} {}", scaled(done as f64, unit), scaled(total as f64, unit), UNITS[unit])
}

fn elapsed(took: Duration) -> String {
    let secs = took.as_secs();
    match secs {
        0..60 => format!("{:.1}s", took.as_secs_f64()),
        60..3600 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, secs % 3600 / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn plain_cp_lines_are_ours() {
        let job = job(&words(&["cp", "-rv", "/backup"])).unwrap();
        assert_eq!(job.dest, PathBuf::from("/backup"));
        assert!(job.verbose && !job.no_clobber);
        assert!(super::job(&words(&["/bin/cp", "-n", "x"])).unwrap().no_clobber);
    }

    #[test]
    fn anything_else_is_left_to_cp() {
        assert_eq!(job(&words(&["cp", "-u", "/backup"])), None, "a flag we don't implement");
        assert_eq!(job(&words(&["cp", "--reflink=auto", "/backup"])), None);
        assert_eq!(job(&words(&["cp"])), None, "no destination");
        assert_eq!(job(&words(&["cp", "stray", "/backup"])), None, "a word we can't place");
        assert_eq!(job(&words(&["rsync", "/backup"])), None);
    }

    #[test]
    fn sizes_read_as_a_fraction_of_the_total() {
        assert_eq!(human(0.0), "0 B");
        assert_eq!(human(1536.0), "1.5 KiB");
        assert_eq!(ratio(1 << 29, 3 << 30), "0.5/3.0 GiB");
    }
}
