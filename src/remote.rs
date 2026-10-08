//! Remote directories, reached over ssh.
//!
//! `magicfs nas:videos` mounts the host and hands the rest of the program a
//! local path, so a remote directory is a source like any other: the view is
//! still a directory of symlinks, and everything that orders, filters and runs
//! commands is none the wiser.
//!
//! One mount per host, of its `/`, under `mnt/` in the views directory. That
//! keeps the mapping between a local path and `host:/path` a matter of
//! stripping a prefix, lets any number of views of one host share a
//! connection, and makes the local path stable — so a seen-list written today
//! still describes the same directory after a remount.
//!
//! The mount is the one thing here that can outlive its purpose, so it is
//! dropped as soon as the last view of the host closes and nothing is using
//! it. Unmounting is never forced on that path: a busy mount is simply left
//! for the next `close`, `unmount` or `clean`.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::invoke::is_program;
use crate::view::{self, View};

/// The directory under the views directory that holds the mounts. Never a
/// view, and never a leftover for `clean` to remove.
pub const MOUNTS: &str = "mnt";

/// `http://...` has the shape of `host:path` and is nothing of the sort.
const SCHEMES: &[&str] = &[
    "http", "https", "ftp", "ftps", "file", "smb", "nfs", "rtsp", "rtmp", "mms", "dav", "davs",
];

/// Handed to ssh so that a host that went away is noticed — a dead mount
/// otherwise hangs whatever touches it — and one that isn't there fails fast.
const KEEPALIVE: [&str; 3] =
    ["ServerAliveInterval=15", "ServerAliveCountMax=3", "ConnectTimeout=10"];

/// A directory on another machine, as scp spells it: `[user@]host:path`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    /// Whatever `ssh` would be given, so `~/.ssh/config` aliases work.
    pub host: String,
    /// Absolute, or relative to the remote home. Empty is the home itself.
    pub path: String,
}

impl Remote {
    /// Read a word as `host:path`, unless it is something local.
    ///
    /// A path that exists here wins, so a file with a colon in its name is
    /// still that file.
    pub fn parse(word: &str) -> Option<Remote> {
        let (host, path) = word.split_once(':')?;
        if host.is_empty()
            || host.starts_with('-')
            || host == "."
            || host == ".."
            || host.contains(|c: char| c == '/' || c.is_whitespace())
        {
            return None;
        }
        if path.starts_with("//") && SCHEMES.contains(&host.to_ascii_lowercase().as_str()) {
            return None;
        }
        if Path::new(word).exists() {
            return None;
        }
        Some(Remote { host: host.to_string(), path: path.to_string() })
    }

    /// The path from the remote `/`, asking the host for its home directory
    /// when the path is relative to it.
    fn absolute(&self) -> Result<String> {
        if self.path.starts_with('/') {
            return Ok(self.path.clone());
        }
        let rest = match self.path.as_str() {
            "~" => "",
            path => path.strip_prefix("~/").unwrap_or(path),
        };
        let home = home(&self.host)?;
        Ok(format!("{}/{rest}", home.trim_end_matches('/')))
    }
}

/// A mounted host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    pub host: String,
    pub point: PathBuf,
}

/// Where the mounts live.
pub fn root() -> PathBuf {
    let dir = view::base_dir().join(MOUNTS);
    dir.canonicalize().unwrap_or(dir)
}

/// Mount the host if it isn't already, and return the local directory that is
/// the remote one.
pub fn open(remote: &Remote) -> Result<PathBuf> {
    let dir = view::base_dir().join(MOUNTS);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let point = root().join(&remote.host);

    // Listed but unreadable is a connection that died: start it over.
    if is_mounted(&point) && std::fs::metadata(&point).is_err() {
        unmount(&point, true);
    }
    if !is_mounted(&point) {
        std::fs::create_dir_all(&point)
            .with_context(|| format!("creating {}", point.display()))?;
        mount(&remote.host, &point)?;
    }

    let path = remote.absolute()?;
    let local = point.join(path.trim_start_matches('/'));
    if !local.is_dir() {
        bail!("{}:{path} is not a directory", remote.host);
    }
    Ok(local)
}

/// A path the way the user would name it: `nas:/home/u/videos` for one inside
/// a mount, itself otherwise.
pub fn shown(path: &Path) -> String {
    let inside = path.strip_prefix(root()).ok().and_then(|rest| {
        let mut parts = rest.components();
        let host = parts.next()?.as_os_str().to_string_lossy().into_owned();
        Some(format!("{host}:/{}", parts.as_path().display()))
    });
    inside.unwrap_or_else(|| path.display().to_string())
}

/// Every host mounted right now.
///
/// Read from the kernel's mount table rather than by looking at the
/// directories, because looking at a mount whose host is gone can hang.
pub fn mounted() -> Vec<Mount> {
    let root = root();
    let mut mounts: Vec<Mount> = mount_points()
        .into_iter()
        .filter(|point| point.parent() == Some(root.as_path()))
        .filter_map(|point| {
            let host = point.file_name()?.to_string_lossy().into_owned();
            Some(Mount { host, point })
        })
        .collect();
    mounts.sort_by(|a, b| a.host.cmp(&b.host));
    mounts.dedup();
    mounts
}

/// Unmount what no view presents any more.
///
/// `keep` names places a shell is about to be sent: it isn't standing there
/// yet, so the kernel would let the mount go from under it.
pub fn release_idle(views: &[View], keep: &[PathBuf]) {
    for mount in mounted() {
        let wanted = views.iter().any(|v| v.source.starts_with(&mount.point))
            || keep.iter().any(|k| k.starts_with(&mount.point));
        if !wanted && unmount(&mount.point, false) {
            eprintln!("unmounted {}", mount.host);
        }
    }
}

/// Unmount, and say whether it went. `force` detaches a mount that is still in
/// use; without it a busy mount is left exactly as it was.
pub fn unmount(point: &Path, force: bool) -> bool {
    let program = if is_program("fusermount3") { "fusermount3" } else { "fusermount" };
    let gone = Command::new(program)
        .arg(if force { "-uz" } else { "-u" })
        .arg(point)
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if gone {
        // Only ever the emptied mount point: `remove_dir` takes nothing with it.
        let _ = std::fs::remove_dir(point);
    }
    gone
}

fn mount(host: &str, point: &Path) -> Result<()> {
    let ssh = ssh_command();
    let custom = std::env::var_os("MAGICFS_SSH").is_some();

    let sshfs = is_program("sshfs");
    let status = if sshfs {
        let mut options = format!("reconnect,follow_symlinks,{}", KEEPALIVE.join(","));
        if custom {
            options.push_str(&format!(",ssh_command={}", ssh.join(" ")));
        }
        Command::new("sshfs")
            .arg(format!("{host}:/"))
            .arg(point)
            .args(["-o", &options])
            .status()
    } else if is_program("rclone") {
        // rclone's own ssh client reads neither ~/.ssh/config nor the agent's
        // idea of which key goes with which host, so it is told to run ssh.
        let mut line = ssh;
        line.extend(KEEPALIVE.iter().map(|o| format!("-o{o}")));
        line.push(host.to_string());
        Command::new("rclone")
            .env("RCLONE_CONFIG_MAGICFS_TYPE", "sftp")
            .env("RCLONE_CONFIG_MAGICFS_SSH", line.join(" "))
            .env("RCLONE_CONFIG_MAGICFS_SHELL_TYPE", "unix")
            .args(["mount", "magicfs:/"])
            .arg(point)
            .args(["--daemon", "--dir-cache-time", "20s", "--log-level", "ERROR"])
            .status()
    } else {
        bail!(
            "mounting {host} needs `sshfs` (or `rclone`), and neither is in your PATH — \
             `apt install sshfs`, or your system's equivalent"
        );
    };

    let status = status.with_context(|| format!("cannot start the mount of {host}"))?;
    // Both daemonise once connected, but give the kernel a moment to list it.
    let mut up = is_mounted(point);
    for _ in 0..30 {
        if up || !status.success() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        up = is_mounted(point);
    }
    if !up {
        let _ = std::fs::remove_dir(point);
        // An rclone too old to be told which ssh to run fails here as well.
        let hint = if sshfs { "" } else { " (without sshfs, this takes a recent rclone)" };
        bail!("cannot mount {host} — does `ssh {host}` work?{hint}");
    }
    eprintln!("mounted {host}");
    Ok(())
}

/// The remote home directory, which `host:videos` is relative to.
///
/// Asked for once per session and kept next to the mounts, since finding out
/// costs a whole ssh connection.
fn home(host: &str) -> Result<String> {
    let cache = root().join(format!(".home.{host}"));
    if let Ok(known) = std::fs::read_to_string(&cache)
        && known.starts_with('/')
    {
        return Ok(known.trim_end().to_string());
    }

    let (program, args) = {
        let mut words = ssh_command();
        let program = words.remove(0);
        (program, words)
    };
    let out = Command::new(&program)
        .args(args)
        .arg(format!("-o{}", KEEPALIVE[2]))
        .arg(host)
        .arg("pwd")
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("cannot run `{program}`"))?;
    // The last line: an rc file that prints a greeting must not become a path.
    let text = String::from_utf8_lossy(&out.stdout);
    let home = text.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    if !out.status.success() || !home.starts_with('/') {
        bail!("cannot find the home directory on {host} — name the path in full, `{host}:/...`");
    }
    let _ = std::fs::write(&cache, home);
    Ok(home.to_string())
}

/// The ssh to use, for the mount and for asking after the home directory.
/// `MAGICFS_SSH` overrides it, the way `GIT_SSH_COMMAND` does for git.
fn ssh_command() -> Vec<String> {
    let words: Vec<String> = std::env::var("MAGICFS_SSH")
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect();
    if words.is_empty() { vec!["ssh".to_string()] } else { words }
}

fn is_mounted(point: &Path) -> bool {
    mount_points().iter().any(|p| p == point)
}

fn mount_points() -> Vec<PathBuf> {
    let table = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    table.lines().filter_map(mount_point).collect()
}

/// The mount point out of one line of `/proc/self/mountinfo` — the fifth
/// field, with whitespace and backslashes written as octal escapes.
fn mount_point(line: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    let field = line.split(' ').nth(4)?.as_bytes();
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0;
    while i < field.len() {
        let octal = field.get(i + 1..i + 4).filter(|_| field[i] == b'\\');
        match octal.and_then(|d| std::str::from_utf8(d).ok()).and_then(|d| u8::from_str_radix(d, 8).ok()) {
            Some(byte) => {
                out.push(byte);
                i += 4;
            }
            None => {
                out.push(field[i]);
                i += 1;
            }
        }
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(host: &str, path: &str) -> Option<Remote> {
        Some(Remote { host: host.to_string(), path: path.to_string() })
    }

    #[test]
    fn a_host_and_a_path_is_a_remote() {
        assert_eq!(Remote::parse("nas:videos"), remote("nas", "videos"));
        assert_eq!(Remote::parse("nas:"), remote("nas", ""));
        assert_eq!(Remote::parse("me@10.0.0.2:/srv/x"), remote("me@10.0.0.2", "/srv/x"));
        // Not scp's spelling, but what people type; the extra slash is harmless.
        assert_eq!(Remote::parse("10.0.0.2://home/u"), remote("10.0.0.2", "//home/u"));
    }

    #[test]
    fn local_things_and_urls_are_not_remotes() {
        for word in ["videos", "/tmp", "./a:b", "dir/a:b", ":x", "-o:x", "https://example.com/a"] {
            assert_eq!(Remote::parse(word), None, "{word}");
        }
        // A file that really is called that is that file.
        let td = crate::testutil::TempDir::new("remote-colon");
        td.mkdir("nas:videos");
        let word = td.path().join("nas:videos");
        assert_eq!(Remote::parse(&word.to_string_lossy()), None);
    }

    #[test]
    fn an_absolute_path_needs_no_trip_to_the_host() {
        let r = Remote { host: "nowhere.invalid".into(), path: "/srv/x".into() };
        assert_eq!(r.absolute().unwrap(), "/srv/x");
    }

    #[test]
    fn paths_inside_a_mount_are_shown_the_way_they_were_asked_for() {
        // Settles where the views directory is before `root` is asked twice.
        let _td = crate::testutil::TempDir::new("remote-shown");
        let inside = root().join("nas").join("home/u/videos");
        assert_eq!(shown(&inside), "nas:/home/u/videos");
        assert_eq!(shown(&root().join("me@nas")), "me@nas:/");
        assert_eq!(shown(Path::new("/home/u/videos")), "/home/u/videos");
    }

    #[test]
    fn mount_points_are_read_with_their_escapes_undone() {
        let line = "50 30 0:67 / /run/user/1000/magicfs/mnt/nas rw,nosuid - fuse.sshfs nas:/ rw";
        assert_eq!(mount_point(line), Some(PathBuf::from("/run/user/1000/magicfs/mnt/nas")));
        let line = r"51 30 0:68 / /mnt/my\040disk\134x rw - ext4 /dev/sda1 rw";
        assert_eq!(mount_point(line), Some(PathBuf::from(r"/mnt/my disk\x")));
    }
}
