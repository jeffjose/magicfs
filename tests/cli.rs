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
            .env("MAGICFS_SEEN_DIR", self.dir.join("seen"))
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

/// What the shell hands us for `magicfs -s time smplayer *`: the glob is gone
/// by the time we run, expanded into names in the shell's own order.
fn expanded(fx: &Fixture) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(&fx.source)
        .unwrap()
        .flatten()
        .map(|d| d.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn an_expanded_glob_becomes_the_view_in_the_right_order() {
    let fx = chronological_fixture("run-glob");
    let names = expanded(&fx);
    let mut args = vec!["-s", "time", "--dry-run", "smplayer"];
    args.extend(names.iter().map(String::as_str));

    let (out, _, ok) = fx.run(&args, &fx.source);
    assert!(ok, "running a command failed");
    // The filenames the shell produced are gone; the view's are there instead,
    // and sorting them the way a shell would reproduces newest-first.
    assert_eq!(
        out,
        "smplayer 001-aaa.jpg 002-bbb.jpg 003-ccc.jpg 004-ddd.png 005-eee.png"
    );
}

#[test]
fn the_commands_own_flags_survive_and_stay_in_front() {
    let fx = chronological_fixture("run-flags");
    let names = expanded(&fx);
    let mut args = vec!["-s", "time", "--dry-run", "smplayer", "--fullscreen", "-Z"];
    args.extend(names.iter().map(String::as_str));

    let (out, _, ok) = fx.run(&args, &fx.source);
    assert!(ok, "running a command failed");
    assert!(
        out.starts_with("smplayer --fullscreen -Z 001-aaa.jpg"),
        "got {out}"
    );
}

#[test]
fn a_partial_glob_narrows_the_view_to_what_it_matched() {
    let fx = chronological_fixture("run-subset");
    // `smplayer *.png` — the shell expands it, so only the PNGs reach us.
    let (out, _, ok) = fx.run(
        &["-s", "time", "--dry-run", "smplayer", "ddd.png", "eee.png"],
        &fx.source,
    );
    assert!(ok);
    assert_eq!(out, "smplayer 001-ddd.png 002-eee.png");
}

#[test]
fn a_quoted_pattern_is_expanded_against_the_view_instead() {
    // Quoting means the shell never touched it, so we do the expansion — and
    // land in exactly the same place.
    let fx = chronological_fixture("run-quoted");
    let (out, _, ok) = fx.run(&["-s", "time", "--dry-run", "smplayer", "*.png"], &fx.source);
    assert!(ok);
    assert_eq!(out, "smplayer 001-ddd.png 002-eee.png");
}

#[test]
fn arguments_that_name_no_file_are_left_where_they_were() {
    // `cp * /backup` must not copy /backup into itself, nor lose it.
    let fx = chronological_fixture("run-dest");
    let names = expanded(&fx);
    // --links, so the names show the order; `cp` would get real paths.
    let mut args = vec!["-s", "time", "--dry-run", "--links", "cp"];
    args.extend(names.iter().map(String::as_str));
    args.push("/backup");

    let (out, _, ok) = fx.run(&args, &fx.source);
    assert!(ok);
    assert_eq!(
        out,
        "cp 001-aaa.jpg 002-bbb.jpg 003-ccc.jpg 004-ddd.png 005-eee.png /backup"
    );
}

#[test]
fn file_tools_are_handed_the_real_files() {
    // A view name would have `rm` delete the link and leave the file.
    let fx = chronological_fixture("real-rm");
    let (out, _, ok) = fx.run(&["-s", "time", "-n", "2", "--dry-run", "rm", "-v"], &fx.source);
    assert!(ok);
    let src = fx.source.display();
    assert_eq!(out, format!("rm -v {src}/aaa.jpg {src}/bbb.jpg"));
}

#[test]
fn rm_deletes_the_real_files_and_builds_no_view() {
    let fx = chronological_fixture("real-rm-run");
    let names = expanded(&fx);
    let mut args = vec!["-s", "time", "-n", "1", "rm"];
    args.extend(names.iter().map(String::as_str));
    // Not at a terminal, so nothing is asked.
    let (_, err, ok) = fx.run(&args, &fx.source);
    assert!(ok, "{err}");
    assert!(!fx.source.join("aaa.jpg").exists(), "the newest file is gone");
    assert!(fx.source.join("bbb.jpg").exists(), "and only that one");
    assert_eq!(std::fs::read_dir(&fx.base).unwrap().count(), 0, "no view left behind");
}

#[test]
fn rm_from_inside_a_view_tidies_the_view() {
    let fx = chronological_fixture("real-rm-inside");
    let (view, _, ok) = fx.run(&["--no-cd", "-s", "time"], &fx.source);
    assert!(ok);
    let view = PathBuf::from(view);
    let (_, err, ok) = fx.run(&["rm", "001-aaa.jpg"], &view);
    assert!(ok, "{err}");
    assert!(!fx.source.join("aaa.jpg").exists());
    assert_eq!(
        fx.glob(&view),
        ["001-bbb.jpg", "002-ccc.jpg", "003-ddd.png", "004-eee.png"],
        "no broken link where the file was"
    );
}

#[test]
fn a_destination_that_is_in_the_directory_stays_the_destination() {
    // `cp * sub/` expands to `... sub sub/`, and sub/ is where they go.
    let fx = chronological_fixture("real-cp-sub");
    std::fs::create_dir(fx.source.join("sub")).unwrap();
    let (out, _, ok) =
        fx.run(&["--dry-run", "--dirs", "exclude", "cp", "aaa.jpg", "sub/"], &fx.source);
    assert!(ok);
    assert_eq!(out, format!("cp {}/aaa.jpg sub/", fx.source.display()));
}

#[test]
fn a_command_naming_no_files_still_receives_the_whole_view() {
    let fx = chronological_fixture("run-bare");
    let (out, _, ok) = fx.run(&["-s", "time", "--dry-run", "mpv", "--loop"], &fx.source);
    assert!(ok);
    assert_eq!(
        out,
        "mpv --loop 001-aaa.jpg 002-bbb.jpg 003-ccc.jpg 004-ddd.png 005-eee.png"
    );
}

#[test]
fn a_command_actually_runs_inside_the_view() {
    let fx = chronological_fixture("run-real");
    let names = expanded(&fx);
    let mut args = vec!["-s", "time", "echo"];
    args.extend(names.iter().map(String::as_str));

    let (out, _, ok) = fx.run(&args, &fx.source);
    assert!(ok, "the command should run, not just be printed");
    assert_eq!(
        out,
        "001-aaa.jpg 002-bbb.jpg 003-ccc.jpg 004-ddd.png 005-eee.png"
    );
}

#[test]
fn the_commands_exit_status_is_ours() {
    let fx = chronological_fixture("run-status");
    let (_, _, ok) = fx.run(&["-s", "time", "false"], &fx.source);
    assert!(!ok, "a failing command must fail the whole invocation");
}

#[test]
fn files_with_no_command_are_just_a_narrower_view() {
    // `magicfs -s time *.png` — nothing to run, so it is a view of the PNGs.
    let fx = chronological_fixture("run-pick");
    let (view, _, ok) = fx.run(&["-s", "time", "ddd.png", "eee.png"], &fx.source);
    assert!(ok);
    assert_eq!(fx.glob(Path::new(&view)), vec!["001-ddd.png", "002-eee.png"]);
}

#[test]
fn a_narrowed_view_survives_being_reordered() {
    let fx = chronological_fixture("run-pick-resort");
    let (view, _, _) = fx.run(&["-s", "time", "ddd.png", "eee.png"], &fx.source);
    let (_, _, ok) = fx.run(&["sort", "name", "-r"], Path::new(&view));
    assert!(ok);
    // Still the two PNGs, now reversed by name.
    assert_eq!(fx.glob(Path::new(&view)), vec!["001-eee.png", "002-ddd.png"]);
}

#[test]
fn a_command_run_from_inside_a_view_reorders_it_first() {
    let fx = chronological_fixture("run-inside");
    let (view, _, _) = fx.run(&["-s", "name"], &fx.source);
    let view = PathBuf::from(&view);
    // `*` in the view expands to the view's own links, which we have to follow
    // back to the real files before we can rename them into the new order.
    let names = fx.glob(&view);
    let mut args = vec!["-s", "time", "-r", "--dry-run", "echo"];
    args.extend(names.iter().map(String::as_str));

    let (out, _, ok) = fx.run(&args, &view);
    assert!(ok);
    assert_eq!(
        out,
        "echo 001-eee.png 002-ddd.png 003-ccc.jpg 004-bbb.jpg 005-aaa.jpg"
    );
}

#[test]
fn a_quoted_command_line_is_handed_to_a_shell_in_the_view() {
    let fx = chronological_fixture("run-shell");
    let (out, _, ok) = fx.run(&["-s", "time", "echo *.png"], &fx.source);
    assert!(ok, "the quoted line should run");
    // The shell expanded the glob itself, in the view.
    assert_eq!(out, "004-ddd.png 005-eee.png");
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
fn a_second_terminal_gets_its_own_view() {
    // Terminal A is standing in its view; terminal B running magicfs on the
    // same directory must not reorder A's contents underneath it.
    let fx = chronological_fixture("two-terminals");
    let (a, _, _) = fx.run(&["-s", "time"], &fx.source);
    let a = PathBuf::from(a);
    let a_before = fx.glob(&a);

    let (b, _, _) = fx.run(&["-s", "size"], &fx.source);
    let b = PathBuf::from(b);

    assert_ne!(a, b, "two invocations must not share a view directory");
    assert_eq!(fx.glob(&a), a_before, "the first view must be untouched");
    assert!(a.is_dir() && b.is_dir(), "both views should be live");
}

#[test]
fn new_opens_a_second_view_to_compare_against() {
    let fx = Fixture::new("new-view");
    for i in 0..30 {
        fx.photo(&format!("f{i:02}.jpg"), "2024-01-01");
    }
    let (first, _, _) = fx.run(&["-s", "random"], &fx.source);
    let first = PathBuf::from(first);
    let a = fx.glob(&first);

    // From inside the first view, --new builds a sibling instead of re-rolling
    // the one you are standing in.
    let (second, stderr, ok) = fx.run(&["-s", "random", "--new"], &first);
    assert!(ok, "got: {stderr}");
    let second = PathBuf::from(second);

    assert_ne!(first, second, "--new must not reuse the current view");
    assert_eq!(fx.glob(&first), a, "the original shuffle must survive");
    assert_ne!(fx.glob(&second), a, "the second view should be a fresh shuffle");
}

#[test]
fn new_inherits_the_filters_of_the_view_it_forks_from() {
    // Comparing two shuffles of "the PNGs" shouldn't silently widen to
    // everything.
    let fx = chronological_fixture("new-inherit");
    let (view, _, _) = fx.run(&["-s", "random", "-f", "png"], &fx.source);
    let (second, stderr, ok) = fx.run(&["-s", "random", "--new"], Path::new(&view));
    assert!(ok, "got: {stderr}");
    assert_eq!(fx.glob(Path::new(&second)).len(), 2, "should still be PNG-only");
}

#[test]
fn clean_needs_confirmation_and_then_removes_everything() {
    let fx = chronological_fixture("clean");
    let (a, _, _) = fx.run(&["-s", "time"], &fx.source);
    let (b, _, _) = fx.run(&["-s", "size"], &fx.source);

    let (_, stderr, ok) = fx.run(&["clean"], &fx.dir);
    assert!(ok);
    assert!(stderr.contains("would close"), "dry run should preview: {stderr}");
    assert!(Path::new(&a).is_dir(), "dry run must not delete anything");

    let (_, stderr, ok) = fx.run(&["clean", "--yes"], &fx.dir);
    assert!(ok, "got: {stderr}");
    assert!(!Path::new(&a).exists() && !Path::new(&b).exists(), "views should be gone");
    assert_eq!(
        std::fs::read_dir(&fx.source).unwrap().count(),
        5,
        "real files must survive"
    );

    let (_, stderr, _) = fx.run(&["clean"], &fx.dir);
    assert!(stderr.contains("nothing to clean"), "got: {stderr}");
}

#[test]
fn clean_sweeps_the_husks_left_by_closing_from_inside() {
    let fx = chronological_fixture("clean-husk");
    let (view, _, _) = fx.run(&["-s", "time"], &fx.source);
    let view = PathBuf::from(view);
    fx.run(&["close"], &view); // leaves the emptied directory behind
    assert!(view.is_dir(), "sanity: close-from-inside keeps the directory");

    fx.run(&["clean", "--yes"], &fx.dir);
    assert!(!view.exists(), "clean should sweep the leftover");
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

/// Run with a `MAGICFS_CD_FILE` wrapper attached and report what, if anything,
/// magicfs asked the shell to cd to.
fn cd_hint(fx: &Fixture, args: &[&str], cwd: &Path, label: &str) -> Option<String> {
    let hint = fx.dir.join(format!("hint-{label}.txt"));
    let ok = Command::new(env!("CARGO_BIN_EXE_magicfs"))
        .args(args)
        .current_dir(cwd)
        .env("MAGICFS_DIR", &fx.base)
        .env("MAGICFS_REGISTRY", &fx.registry)
        .env("MAGICFS_CD_FILE", &hint)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "magicfs {args:?} failed");
    std::fs::read_to_string(&hint).ok().filter(|s| !s.is_empty())
}

#[test]
fn demo_lands_you_in_the_directory_it_creates() {
    // Otherwise the first thing the demo asks of you is a manual cd.
    let fx = Fixture::new("demo-cd");
    let target = fx.dir.join("demo");
    let args = ["demo", "--out", target.to_str().unwrap()];
    assert_eq!(
        cd_hint(&fx, &args, &fx.dir, "demo").as_deref(),
        Some(target.to_str().unwrap()),
        "demo should hand the shell its sample directory"
    );
}

#[test]
fn running_a_command_leaves_the_shell_in_the_view_too() {
    // The command ran in the view, so that is where `cd -` should return from.
    let fx = chronological_fixture("run-cd");
    let hint = cd_hint(&fx, &["-s", "time", "true"], &fx.source, "run");
    assert!(
        hint.is_some_and(|h| Path::new(&h).is_dir()),
        "a command invocation should still hand the shell the view"
    );
    assert_eq!(
        cd_hint(&fx, &["--no-cd", "-s", "time", "true"], &fx.source, "run-off"),
        None,
        "--no-cd must suppress that too"
    );
}

#[test]
fn no_cd_leaves_the_shell_alone() {
    let fx = chronological_fixture("no-cd");
    assert!(
        cd_hint(&fx, &["-s", "time"], &fx.source, "on").is_some(),
        "sanity: a view command normally moves you"
    );
    assert_eq!(
        cd_hint(&fx, &["--no-cd", "-s", "size"], &fx.source, "off"),
        None,
        "--no-cd must suppress the cd entirely"
    );
}

#[test]
fn read_only_commands_never_move_you() {
    // `paths`/`exec`/`list` answer a question; moving the shell would be a
    // side effect nobody asked for.
    let fx = chronological_fixture("no-move");
    fx.run(&["-s", "time"], &fx.source);
    for (args, label) in [
        (vec!["paths"], "paths"),
        (vec!["exec", "--dry-run", "echo"], "exec"),
        (vec!["list"], "list"),
    ] {
        assert_eq!(cd_hint(&fx, &args, &fx.source, label), None, "{label} moved the shell");
    }
}

/// `magicfs -s time --unseen ... echo *` from the source directory, the way
/// the shell would hand it over.
fn unseen_echo(fx: &Fixture, extra: &[&str]) -> (String, String, bool) {
    let names = expanded(fx);
    let mut args = vec!["-s", "time", "--unseen"];
    args.extend_from_slice(extra);
    args.push("echo");
    args.extend(names.iter().map(String::as_str));
    fx.run(&args, &fx.source)
}

#[test]
fn unseen_hands_over_new_files_and_then_skips_them() {
    let fx = chronological_fixture("unseen-batches");
    let (out, err, ok) = unseen_echo(&fx, &["-n", "2"]);
    assert!(ok, "{err}");
    assert_eq!(out, "001-aaa.jpg 002-bbb.jpg");
    assert!(err.contains("marked 2 seen"), "should say it marked them: {err}");

    // The limit counts unseen files, so the next batch is the next two.
    let (out, _, ok) = unseen_echo(&fx, &["-n", "2"]);
    assert!(ok);
    assert_eq!(out, "001-ccc.jpg 002-ddd.png");
}

#[test]
fn when_nothing_is_new_the_command_does_not_run() {
    // The expanded glob names every file, and "naming everything" must not
    // fall back to handing over the whole directory again.
    let fx = chronological_fixture("unseen-empty");
    assert!(unseen_echo(&fx, &[]).2);

    let (out, err, ok) = unseen_echo(&fx, &[]);
    assert!(!ok, "caught up should exit non-zero");
    assert!(out.is_empty(), "echo must not have run: {out}");
    assert!(err.contains("nothing new") && err.contains("5 seen"), "got: {err}");
    // No empty view left behind to cd into.
    assert_eq!(std::fs::read_dir(&fx.base).unwrap().count(), 1, "only the first view");
}

#[test]
fn a_dry_run_marks_nothing() {
    let fx = chronological_fixture("unseen-dry");
    assert!(unseen_echo(&fx, &["--dry-run"]).2);
    let (out, _, ok) = unseen_echo(&fx, &[]);
    assert!(ok);
    assert_eq!(out.split(' ').count(), 5);
}

#[test]
fn unsee_last_puts_the_latest_batch_back() {
    let fx = chronological_fixture("unseen-undo");
    assert!(unseen_echo(&fx, &["-n", "2"]).2);
    assert!(unseen_echo(&fx, &["-n", "2"]).2);

    let (_, err, ok) = fx.run(&["unsee", "--last"], &fx.source);
    assert!(ok, "{err}");
    let (out, _, _) = unseen_echo(&fx, &["-n", "2"]);
    assert_eq!(out, "001-ccc.jpg 002-ddd.png", "the second batch is back, the first is not");
}

#[test]
fn seen_star_starts_from_now() {
    let fx = chronological_fixture("unseen-bootstrap");
    let names = expanded(&fx);
    let mut args = vec!["seen"];
    args.extend(names.iter().map(String::as_str));
    let (_, err, ok) = fx.run(&args, &fx.source);
    assert!(ok, "{err}");

    fx.photo("fff.mp4", "2024-02-01");
    let (out, _, ok) = unseen_echo(&fx, &[]);
    assert!(ok);
    assert_eq!(out, "001-fff.mp4");
}

#[test]
fn unsee_refuses_a_name_that_is_not_a_file() {
    let fx = chronological_fixture("unseen-typo");
    let (_, err, ok) = fx.run(&["unsee", "nope.mp4"], &fx.source);
    assert!(!ok);
    assert!(err.contains("nope.mp4"), "got: {err}");
}

#[test]
fn a_misspelt_file_is_named_back_with_what_it_probably_meant() {
    // Not at a terminal, so nothing is asked — but the error says what to type.
    let fx = chronological_fixture("typo-pick");
    let (_, err, ok) = fx.run(&["--no-cd", "bbc.jpg"], &fx.source);
    assert!(!ok);
    assert!(err.contains("did you mean `bbb.jpg`"), "got: {err}");

    let (_, err, ok) = fx.run(&["unsee", "ddd.pgn"], &fx.source);
    assert!(!ok);
    assert!(err.contains("did you mean `ddd.png`"), "got: {err}");
}

#[test]
fn latest_and_oldest_pick_by_time() {
    let fx = chronological_fixture("latest");
    let (out, _, ok) = fx.run(&["--latest", "--dry-run", "echo"], &fx.source);
    assert!(ok);
    assert_eq!(out, "echo 001-aaa.jpg");
    let (out, _, ok) = fx.run(&["--oldest", "-n", "2", "--dry-run", "echo"], &fx.source);
    assert!(ok);
    assert_eq!(out, "echo 001-eee.png 002-ddd.png");
}

fn days_fixture(label: &str) -> Fixture {
    let fx = Fixture::new(label);
    fx.photo("now.png", "now");
    fx.photo("yday-am.png", "yesterday 09:00");
    fx.photo("yday-pm.png", "yesterday 15:00");
    fx.photo("old.png", "2020-01-01");
    fx
}

#[test]
fn when_keeps_only_that_stretch_of_time() {
    let fx = days_fixture("when");
    let run = |w: &str| {
        let (out, err, ok) = fx.run(&["-s", "time", "-w", w, "--dry-run", "echo"], &fx.source);
        assert!(ok, "{w}: {err}");
        out
    };
    assert_eq!(run("yesterday"), "echo 001-yday-pm.png 002-yday-am.png");
    assert_eq!(run("yesterday.morning"), "echo 001-yday-am.png");
    assert_eq!(run("1h"), "echo 001-now.png");
    assert_eq!(run("today,2020-01-01"), "echo 001-now.png 002-old.png");
}

#[test]
fn a_misspelt_window_is_refused_before_anything_runs() {
    let fx = days_fixture("when-typo");
    let (_, err, ok) = fx.run(&["-w", "yesterdya", "--dry-run", "echo"], &fx.source);
    assert!(!ok);
    assert!(err.contains("did you mean `yesterday`"), "{err}");
}

#[test]
fn sessions_are_listed_and_picked_by_number() {
    let fx = days_fixture("sessions");
    fx.photo("yday-pm2.png", "yesterday 15:20");
    let (out, err, ok) = fx.run(&["sessions"], &fx.source);
    assert!(ok, "{err}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 4, "{out}");
    assert!(lines[0].starts_with("@0") && lines[0].contains("today"), "{out}");
    assert!(lines[1].contains("yesterday") && lines[1].contains("15:00–15:20"), "{out}");
    assert!(lines[1].contains("2 files"), "{out}");

    let (out, _, ok) = fx.run(&["-s", "time", "-w", "@1", "--dry-run", "echo"], &fx.source);
    assert!(ok);
    assert_eq!(out, "echo 001-yday-pm2.png 002-yday-pm.png");

    // Within a day, the numbering starts again.
    let (out, _, ok) = fx.run(&["sessions", "-w", "yesterday"], &fx.source);
    assert!(ok);
    assert!(out.lines().nth(1).unwrap().starts_with("yesterday@1"), "{out}");
    let (out, _, ok) = fx.run(&["-w", "yesterday@1", "--dry-run", "echo"], &fx.source);
    assert!(ok);
    assert_eq!(out, "echo 001-yday-am.png");
}

#[test]
fn created_goes_by_when_a_file_was_made_not_last_written() {
    let fx = Fixture::new("created");
    // Made just now, but stamped as written in 2020 — a copied-in photo.
    fx.photo("copied.png", "2020-01-01");
    if std::fs::metadata(fx.source.join("copied.png")).unwrap().created().is_err() {
        return; // this filesystem keeps no creation time
    }
    let (_, _, ok) = fx.run(&["-w", "today", "--dry-run", "echo"], &fx.source);
    assert!(!ok, "by write time it is from 2020");
    let (out, err, ok) = fx.run(&["-w", "today", "--created", "--dry-run", "echo"], &fx.source);
    assert!(ok, "{err}");
    assert_eq!(out, "echo 001-copied.png");
}
