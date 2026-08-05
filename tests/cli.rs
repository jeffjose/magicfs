//! End-to-end tests against the built binary.
//!
//! These assert the property the whole tool exists for: after building a view,
//! sorting its entry names the way a *shell* would must reproduce the
//! requested order.

use std::path::{Path, PathBuf};
use std::process::Command;

struct Fixture {
    dir: PathBuf,
    source: PathBuf,
    base: PathBuf,
    registry: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("magicfs-cli-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let source = dir.join("photos");
        std::fs::create_dir_all(&source).unwrap();
        let fx = Fixture {
            base: dir.join("views"),
            registry: dir.join("registry.json"),
            source,
            dir,
        };
        std::fs::create_dir_all(&fx.base).unwrap();
        fx
    }

    /// Create a file whose alphabetical rank and mtime rank disagree.
    fn photo(&self, name: &str, mtime: &str) {
        let path = self.source.join(name);
        std::fs::write(&path, name.as_bytes()).unwrap();
        let ok = Command::new("touch")
            .args(["-d", mtime])
            .arg(&path)
            .status()
            .unwrap()
            .success();
        assert!(ok, "touch failed for {name}");
    }

    fn run(&self, args: &[&str], cwd: &Path) -> (String, String, bool) {
        let out = Command::new(env!("CARGO_BIN_EXE_magicfs"))
            .args(args)
            .current_dir(cwd)
            .env("MAGICFS_DIR", &self.base)
            .env("MAGICFS_REGISTRY", &self.registry)
            .env_remove("MAGICFS_CD_FILE")
            .output()
            .expect("failed to run magicfs");
        (
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
            out.status.success(),
        )
    }

    /// Exactly what `*` would expand to in the view: names, sorted by the shell.
    fn glob(&self, view: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(view)
            .unwrap()
            .flatten()
            .map(|d| d.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.'))
            .collect();
        names.sort();
        names
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Newest-first, with filenames whose alphabetical order is the exact reverse.
fn chronological_fixture(label: &str) -> Fixture {
    let fx = Fixture::new(label);
    fx.photo("aaa.jpg", "2024-01-05");
    fx.photo("bbb.jpg", "2024-01-04");
    fx.photo("ccc.jpg", "2024-01-03");
    fx.photo("ddd.png", "2024-01-02");
    fx.photo("eee.png", "2024-01-01");
    fx
}

#[test]
fn glob_order_in_the_view_matches_the_requested_order() {
    let fx = chronological_fixture("glob-order");
    let (view, _, ok) = fx.run(&["-s", "time"], &fx.source);
    assert!(ok, "view creation failed");

    let names = fx.glob(Path::new(&view));
    assert_eq!(
        names,
        vec!["001-aaa.jpg", "002-bbb.jpg", "003-ccc.jpg", "004-ddd.png", "005-eee.png"]
    );

    // Follow each link and confirm the real files really are newest-first.
    let targets: Vec<String> = names
        .iter()
        .map(|n| {
            std::fs::read_link(Path::new(&view).join(n))
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(targets, vec!["aaa.jpg", "bbb.jpg", "ccc.jpg", "ddd.png", "eee.png"]);
}

#[test]
fn reverse_flips_the_view_in_place() {
    let fx = chronological_fixture("reverse");
    let (view, _, _) = fx.run(&["-s", "time"], &fx.source);
    let view = PathBuf::from(view);

    let (_, _, ok) = fx.run(&["reverse"], &view);
    assert!(ok);
    assert_eq!(fx.glob(&view)[0], "001-eee.png", "oldest should now be first");
}

#[test]
fn filtering_restricts_what_a_glob_would_see() {
    let fx = chronological_fixture("filter");
    let (view, _, _) = fx.run(&["-s", "time"], &fx.source);
    let view = PathBuf::from(view);

    let (_, _, ok) = fx.run(&["filter", "png"], &view);
    assert!(ok);
    assert_eq!(fx.glob(&view), vec!["001-ddd.png", "002-eee.png"]);

    // Clearing restores the full set.
    fx.run(&["clear"], &view);
    assert_eq!(fx.glob(&view).len(), 5);
}

#[test]
fn random_reorders_and_re_rolls_each_time() {
    let fx = Fixture::new("shuffle");
    for i in 0..30 {
        fx.photo(&format!("f{i:02}.jpg"), "2024-01-01");
    }
    let (view, _, _) = fx.run(&[], &fx.source);
    let view = PathBuf::from(view);
    let alphabetical = fx.glob(&view);

    // There is no `shuffle` subcommand: `-s random` is the shuffle, and asking
    // again re-rolls it.
    fx.run(&["-s", "random"], &view);
    let first = fx.glob(&view);
    fx.run(&["-s", "random"], &view);
    let second = fx.glob(&view);

    assert_ne!(first, alphabetical, "random should not match name order");
    assert_ne!(first, second, "a second `-s random` should re-roll");
    assert_eq!(first.len(), 30);
}

#[test]
fn a_rebuild_that_is_not_a_re_sort_preserves_the_shuffle() {
    // Adding a filter to a shuffled view must not scramble what you were
    // looking at.
    let fx = Fixture::new("shuffle-stable");
    for i in 0..30 {
        fx.photo(&format!("f{i:02}.jpg"), "2024-01-01");
    }
    let (view, _, _) = fx.run(&["-s", "random"], &fx.source);
    let view = PathBuf::from(view);
    let before = fx.glob(&view);

    fx.run(&["refresh"], &view);
    assert_eq!(fx.glob(&view), before, "refresh must keep the shuffle");
}

#[test]
fn reordering_a_plain_directory_opens_a_view_of_it() {
    // `cd dir; magicfs -s random` — no view to set up first, no `.` to type.
    let fx = chronological_fixture("implicit-cwd");
    let (view, stderr, ok) = fx.run(&["-s", "random"], &fx.source);
    assert!(ok, "got: {stderr}");
    assert_eq!(fx.glob(Path::new(&view)).len(), 5);
}

#[test]
fn subcommands_also_open_a_view_when_run_outside_one() {
    let fx = chronological_fixture("implicit-sub");
    let (view, stderr, ok) = fx.run(&["filter", "png"], &fx.source);
    assert!(ok, "got: {stderr}");
    assert_eq!(fx.glob(Path::new(&view)), vec!["001-ddd.png", "002-eee.png"]);
}

#[test]
fn output_capture_suppresses_the_subshell() {
    // Everything here runs with stdout piped, so magicfs must print a path and
    // exit rather than exec a shell — otherwise `$(magicfs .)` would hang.
    let fx = chronological_fixture("no-subshell");
    let (view, _, ok) = fx.run(&["-s", "time"], &fx.source);
    assert!(ok);
    assert!(Path::new(&view).is_dir(), "stdout should be just the view path");
}

#[test]
fn a_reconfigured_view_stays_at_the_same_path() {
    // The user cds in once; later commands must not move the directory out
    // from under their shell.
    let fx = chronological_fixture("stable-path");
    let (view, _, _) = fx.run(&["-s", "time"], &fx.source);
    let (again, _, _) = fx.run(&["sort", "size"], Path::new(&view));
    assert_eq!(view, again);
}

#[test]
fn exec_passes_original_names_in_our_order() {
    let fx = chronological_fixture("exec");
    let (out, _, ok) = fx.run(&["exec", "-s", "time", "--dry-run", "viewer"], &fx.source);
    assert!(ok, "exec failed");

    let parts: Vec<&str> = out.split_whitespace().collect();
    assert_eq!(parts[0], "viewer");
    let names: Vec<&str> = parts[1..]
        .iter()
        .map(|p| p.rsplit('/').next().unwrap())
        .collect();
    // Original filenames, untouched, in newest-first order.
    assert_eq!(names, vec!["aaa.jpg", "bbb.jpg", "ccc.jpg", "ddd.png", "eee.png"]);
}

#[test]
fn paths_emits_ordered_real_paths() {
    let fx = chronological_fixture("paths");
    let (out, _, ok) = fx.run(&["paths", "-s", "time", "-f", "png"], &fx.source);
    assert!(ok);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].ends_with("/ddd.png"), "got {}", lines[0]);
    assert!(lines[1].ends_with("/eee.png"), "got {}", lines[1]);
}

#[test]
fn close_removes_the_view_and_leaves_the_source_untouched() {
    let fx = chronological_fixture("close");
    let (view, _, _) = fx.run(&["-s", "time"], &fx.source);
    let view = PathBuf::from(&view);
    assert!(view.exists());

    // From the source directory: nothing is standing in the view, so it goes
    // entirely. This is where you are after leaving an auto-opened subshell.
    let (_, stderr, ok) = fx.run(&["close"], &fx.source);
    assert!(ok, "close from the source directory failed: {stderr}");
    assert!(!view.exists(), "view directory should be gone");
    assert_eq!(
        std::fs::read_dir(&fx.source).unwrap().count(),
        5,
        "source files must survive"
    );
}

#[test]
fn closing_the_view_you_stand_in_leaves_the_directory_walkable() {
    // Without a wrapper to cd it, the calling shell cannot be moved out — and
    // deleting its cwd makes every later command fail on getcwd.
    let fx = chronological_fixture("close-inside");
    let (view, _, _) = fx.run(&["-s", "time"], &fx.source);
    let view = PathBuf::from(&view);

    let (_, stderr, ok) = fx.run(&["close"], &view);
    assert!(ok, "got: {stderr}");
    assert!(view.is_dir(), "cwd must stay valid: {stderr}");
    assert_eq!(fx.glob(&view).len(), 0, "but the links must be gone");
    assert!(
        stderr.contains(fx.source.to_str().unwrap()),
        "should say where to get back to: {stderr}"
    );
}

#[test]
fn view_only_commands_outside_a_view_explain_themselves() {
    // `status` cannot invent a view the way `-s time` can, so it still errors —
    // but its advice must be runnable in any shell, not just bash.
    let fx = Fixture::new("no-view");
    let (_, stderr, ok) = fx.run(&["status"], &fx.dir);
    assert!(!ok, "status outside a view should fail");
    assert!(stderr.contains("not inside a magicfs view"), "got: {stderr}");
    assert!(
        !stderr.contains("$("),
        "hint uses bash-only syntax that tcsh rejects: {stderr}"
    );
}

#[test]
fn refuses_to_build_a_view_of_a_view() {
    let fx = chronological_fixture("nested");
    let (view, _, _) = fx.run(&["-s", "time"], &fx.source);
    let view = PathBuf::from(view);

    let (_, stderr, ok) = fx.run(&[view.to_str().unwrap()], &fx.dir);
    assert!(!ok, "should refuse");
    assert!(stderr.contains("inside the magicfs view"), "got: {stderr}");
}

#[test]
fn the_shell_wrapper_receives_the_view_path() {
    let fx = chronological_fixture("cd-hint");
    let hint = fx.dir.join("cd-hint.txt");
    let out = Command::new(env!("CARGO_BIN_EXE_magicfs"))
        .args(["-s", "time"])
        .current_dir(&fx.source)
        .env("MAGICFS_DIR", &fx.base)
        .env("MAGICFS_REGISTRY", &fx.registry)
        .env("MAGICFS_CD_FILE", &hint)
        .output()
        .unwrap();
    assert!(out.status.success());

    let written = std::fs::read_to_string(&hint).unwrap();
    let printed = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(written, printed, "cd hint must match the printed path");
    assert!(Path::new(&written).is_dir());
}
