# magicfs

Present a directory in whatever order you want, so that `*` expands to it.

```console
$ cd ~/photos
$ magicfs -s time                # newest-first — puts you in the view and lists it
$ feh *                          # opens newest-first
$ magicfs -s random              # reshuffle
$ feh *                          # now random
$ magicfs filter png             # restrict to PNGs
$ feh *                          # only PNGs, still shuffled
$ magicfs close                  # back to ~/photos, real names
```

There is nothing to install or configure first: `magicfs` on a plain directory
opens a view of it *and* moves you there. `--no-cd` opts out. See
[Getting into the view](#getting-into-the-view) for how, and how to get back.

Or say it in one line — trailing words are a command to run in the view:

```console
$ magicfs -s random smplayer *   # shuffles ~/videos, plays it in that order
```

You don't have to quote the `*`. See [Running a command](#running-a-command).

## Why the filenames change

The obvious design — a filesystem that returns entries in a custom order —
**cannot work**, and it's worth knowing why before reaching for one.

The shell expands `*` by reading the directory and then sorting the names
*itself*, in its own memory, before the command ever runs:

```console
$ ls -U                              # raw readdir order, straight from ext4
banana  zebra  kiwi  mango  apple

$ strace -e getdents64 bash -c 'echo *'
getdents64(3, ... /* 7 entries */)   # filesystem is consulted...
getdents64(3, ... /* 0 entries */)   # ...and is now finished
apple banana kiwi mango zebra        # sorted afterwards, in bash's memory
```

POSIX mandates that sort, and bash, zsh, and tcsh all do it. The filesystem's
only lever is the order of entries in that `getdents64` buffer — and `*`
discards exactly that. No filesystem can influence it: not ext4, not FUSE, not
one you write yourself. `feh` compounds this by sorting directory contents
itself, so even `feh somedir/` ignores readdir order.

So the only lever that actually exists is **the names**. magicfs gives each
file an index prefix chosen so that alphabetical order *is* your order:

```
001-IMG_2934.jpg   002-IMG_0011.jpg   003-DSC_881.jpg
```

The extension is preserved, so `*.png` still works.

## Running a command

Anything after the options is a command, run inside the view once it is built:

```console
$ magicfs -s random smplayer *          # shuffled, and played in that order
$ magicfs -s time feh --scale-down *    # feh's flags are feh's
$ magicfs -s random mpv *.mp4           # only the mp4s, shuffled
```

The `*` needs no quoting, which is the whole point — with
`alias mfr='magicfs -s random'`, `mfr smplayer *` is the entire interaction.

That takes some work, because the shell expands the glob *before* magicfs runs:
what actually arrives is `smplayer a.mp4 b.mp4 c.mp4`, the right files in the
wrong order. So arguments that name files in the directory are recognised as the
glob's output, taken back out, and replaced with the view's names in the slot
they came from. Everything else is left exactly where it was:

| You type | The command gets |
| --- | --- |
| `mfr smplayer *` | `smplayer 001-c.mp4 002-a.mp4 003-b.mp4` |
| `mfr smplayer --fullscreen *` | `smplayer --fullscreen 001-c.mp4 ...` — the flag is kept |
| `mfr smplayer *.mp4` | only the mp4s, and the view holds only them |
| `mfr cp * /backup` | `cp ~/v/c.mp4 ... /backup` — real files, the destination stays last |
| `mfr mpv` | the whole view, in order — naming no files means all of them |

Quoting sidesteps the guesswork entirely, and lands in the same place:
`mfr smplayer '*.mp4'` arrives unexpanded and is matched against the source
directory here, and `mfr 'mpv --loop *'` — one quoted argument, a whole command
line — is handed to a shell *inside* the view, which expands the glob there.

A quoted pattern ignores case, the same as `-f` (`--case-sensitive` turns that
off), and takes braces: `mfr feh '*cat*.{png,jpg}'` finds `Cat-2.PNG` too. In
tcsh and zsh, an unquoted glob that matches nothing stops the shell before
magicfs runs ("No match."), so quote the ones you aren't sure of.

A file name one slip away from a real one is offered back, the way tcsh's
`set correct` does:

```console
$ mfr feh bat.png
CORRECT>cat.png (y|n|a)?
```

`y` (or Enter) uses it, `n` keeps what you typed, `a` stops. Only words that
look like file names are considered — never `/backup`, `--loop` or `50` — and
when two files are equally close nothing is guessed. Away from a terminal
nothing is asked, and a file-only command line fails with `did you mean
cat.png?` instead.

Words that name nothing are none of our business (`/backup` above, or a `--flag`
that belongs to the tool). The command replaces magicfs, so it owns the terminal
and its exit status is the one you get; `--dry-run` prints the line instead.

### Aliases

The command can be one of your shell aliases — `magicfs -s time ll *`, or just
`magicfs -s time 'myalias'` — and it means what it means at your prompt:

- A plain alias (`ll` → `ls -lGh`, `del` → `rm -rf`) is expanded by magicfs,
  following aliases of aliases and tcsh's `\!*`. So `del *` is still `rm`: it
  gets real paths and asks first.
- Anything only a shell can run — a pipe, `;`, `$var` — is handed to your shell
  along with your alias definitions, and runs in the view.

With the `shell-init` wrapper installed, your aliases come with every call, as
they are at that moment, and an alias beats a program of the same name, as it
does at your prompt. Without the wrapper, magicfs has to start a shell to read
your rc file (for tcsh, `~/.cshrc` is sourced with `prompt` set, so an `if ( !
$?prompt ) exit` guard doesn't skip the aliases). That takes a moment, so it
happens only when the command isn't a program in your `PATH` at all.
`MAGICFS_ALIASES=off` turns alias lookup off.

### Commands that manage files

`rm`, `mv`, `cp`, `ln`, `rsync`, `chmod`, `touch`, `tar`, `zip`, `trash`,
`gio trash` and the like are handed the **real paths**, not the view's names —
a view name is a symlink, so `rm 001-cat.png` would delete the link and leave
the file, and `cp` would make a copy called `001-cat.png`. They run where you
typed them, so `cp * backup/` means the `backup/` next to you, and they build
no view and don't move you:

```console
$ magicfs -s time -n 1 rm *            # delete the newest file
rm 1 file in ~/renders:
  final-v3.mp4
proceed? [y/N]
```

Anything that deletes asks first, listing what it is about to remove — the
selection was computed, so this is the first time you see it. `-y` skips the
question, `--dry-run` prints the command instead, and away from a terminal
nothing is asked. Run from inside a view, the view is tidied afterwards so a
deleted file doesn't linger as a broken link.

For a destination that is itself in the directory (`cp * sub/`), the last word
stays the destination. `--real` hands any other command real paths, and
`--links` hands even `cp` the view's names.

Files with no command at all are just a narrower view:

```console
$ magicfs -s time *.jpg          # a view of the JPEGs alone, newest first
```

## By time

`--latest` is the newest file and `--oldest` the oldest — `-s time -n 1`, and
the same with `-r`. `-n` asks for more than one:

```console
$ magicfs --latest feh *           # the screenshot you just took
$ magicfs --oldest -n 3 rm *       # the three oldest, after asking
```

`-w/--when WINDOW` keeps only the files from a stretch of time, going by when
each was last written. It works alongside everything else, so `*` means "the
files from then":

```console
$ magicfs -w today feh *                  # today's, in name order
$ magicfs -w yesterday.morning -s time mpv *
$ magicfs -w 2h -s random feh *.png       # the last two hours' PNGs, shuffled
$ magicfs -w lastweek rm *                # asks first
```

| Window | Means |
| --- | --- |
| `today` `yesterday` | midnight to midnight, local time |
| `mon` … `sun` | the latest one — today, if today is that day |
| `week` `lastweek` `month` `lastmonth` | weeks start on Monday |
| `2026-09-24` `09-24` | that day (this year, if no year) |
| `30m` `2h` `3d` `1w` | the last that long |
| `morning` `afternoon` `evening` `night` | 5–12, 12–17, 17–22, 22–5; alone, the latest one that has started |
| `yesterday.morning` `fri.night` | part of a particular day; night runs into the next morning |
| `14:00..16:30` `yesterday.22..2` | clock times on one day (today unless named) |
| `mon..wed` `09-20..` | whole days, either end open |
| `mon,wed` | either |
| `@0` `@1..@3` `yesterday@0` | sessions — see below |

### Sessions

"Yesterday morning's coding session" rarely started at 5am or stopped at noon,
so the clock can't find it — but the files can. A **session** is a burst of
files with no quiet stretch over 45 minutes between them, and `magicfs
sessions` lists them, latest first:

```console
$ magicfs sessions
@0            today      10:02–11:40      18 files  1h38m
@1            yesterday  15:10–17:45      31 files  2h35m
@2            yesterday  09:00–11:55      40 files  2h55m
@3            Tue 09-22  21:30             1 file
$ magicfs -w @2 -s time feh *             # yesterday morning's, in order
$ magicfs -w @0..@1 rm *                  # the last two, after asking
```

Under `-w`, the numbering starts again inside the window, so yesterday's
sessions are `yesterday@0` (the latest) and `yesterday@1` — `magicfs sessions
-w yesterday` lists them that way, and `-w yesterday@1` picks one. Sessions are
found among the files that survive `-f`/`-x`, so `sessions -f png` are the
bursts of PNGs. `MAGICFS_SESSION_GAP=20m` changes the gap.

### Written or created

Windows and sessions go by when a file was last *written*, which every
filesystem records and which, for screenshots and renders, is when it was
made. `--created` goes by creation time instead — the difference shows for a
file edited later, or one copied in with its old timestamp kept (`cp -p`,
`rsync -t`, a camera import). ext4, btrfs and xfs record it; elsewhere
`--created` falls back to the write time. `-s created` sorts by it.

The window is kept as written and re-read whenever the view is rebuilt, so a
view of `today` is still today's files after midnight. `-w all` or `magicfs
clear` removes it.

## Reviewing only what's new

For a directory that fills up while you watch it (a render finishing one video
at a time, say), `-u/--unseen` shows only the files no command has been handed
yet, and marks the ones it hands over:

```console
$ magicfs -s time -r -u -n 5 smplayer *   # the next 5 new videos, oldest first
marked 5 seen — `magicfs unsee --last` puts them back
$ magicfs -s time -r -u -n 5 smplayer *   # the 5 after those
$ magicfs -s time -r -u smplayer *        # caught up
magicfs: nothing new in ~/renders (43 seen, last marked 2m ago)
```

When nothing is new, the command doesn't run, no view is created, and the exit
status is 1, so `&&` and shell loops do the right thing. The usual rule that
naming every file means "the whole view" doesn't apply here: that would replay
everything you've already watched.

- **Seen means "handed to a command by magicfs".** atime would need no state,
  but `noatime` mounts never update it, and thumbnailers and backups update it
  for files nobody looked at.
- **Files are marked at launch.** The command replaces magicfs, so there is no
  "after" to wait for. If you quit halfway through a batch, `magicfs unsee
  --last` puts the whole batch back.
- **A changed file is new again.** Records are keyed on name, mtime and size,
  so a re-render, or a file that was still being written when it was handed
  over, comes back once it changes.
- `-n` counts unseen files, so `-n 5` is five you haven't seen. `-s time` is
  newest first, so add `-r` to go through a render in the order it finished.

`magicfs seen *` marks everything that's already there, so the next `--unseen`
shows only what arrives after it. `magicfs seen` with no files reports the
count. `magicfs unsee FILE...` and `magicfs unsee --all` forget files.
`--dry-run` marks nothing.

The lists live in `$XDG_STATE_HOME/magicfs/seen/` (default
`~/.local/state/magicfs/seen/`), one file per source directory, never in the
directory itself. `magicfs clean` leaves them alone.

## Keeping the original filenames

There is one ordering nothing downstream re-sorts: an explicit argument list.
If your tool accepts one, magicfs can skip the renaming entirely.

```console
$ magicfs exec -s time feh          # runs: feh ~/photos/golf.jpg ~/photos/foxtrot.jpg ...
$ magicfs paths -s random | feh -f -
$ magicfs paths -0 -f images | xargs -0 my-tool
```

Both preserve the real filenames and create nothing on disk. `exec` works
whether or not you have a view open.

## How the view is built

A view is a plain directory of symlinks under `$XDG_RUNTIME_DIR/magicfs/`, named
after the source with a short random id — `photos-4dk`. No mount, no daemon, no
root, nothing to leak — and because the kernel resolves a symlink once, reads
afterwards run at full native speed. Reconfiguring only touches the links that
actually moved (20,000 files: ~70ms to build, ~150ms to reshuffle).

**Every invocation gets its own view.** Opening `~/photos` twice yields
`photos-4dk` and `photos-q7f`, and an id is never handed out twice. Reuse would
mean a second terminal silently reordering the directory the first one is
standing in, and two unrelated directories that happen to share a basename
would fight over the same name.

Once you *are* in a view, commands reconfigure it in place — unless you pass
`--new`, which forks a second view off the current one instead. That is how you
hold two orderings of the same directory at once:

```console
$ magicfs -s random             # one shuffle
$ magicfs -s random --new       # a second, to compare against — the first is untouched
```

The fork inherits the ordering and filters it came from, so comparing two
shuffles of "the PNGs" stays PNG-only.

Views cost nothing but symlinks, so let them pile up and run `magicfs clean`
when you want the space back in `magicfs list`.

Views never modify or delete your real files. `magicfs close` and `magicfs
clean` remove links only.

## Commands

| Command | Effect |
| --- | --- |
| `magicfs [DIR] [OPTS]` | Open a view of `DIR` (default: the current directory), or reconfigure the one you're in |
| `magicfs [OPTS] CMD...` | ...and run `CMD` in it — see [Running a command](#running-a-command) |
| `magicfs sort KEY` | `name`, `natural`, `time`, `created`, `ctime`, `atime`, `size`, `ext`, `random` |
| `magicfs reverse` | Flip the current order |
| `magicfs filter PAT...` | Restrict the view; no args clears |
| `magicfs exclude PAT...` | Drop matching entries |
| `magicfs limit N` | Keep the first N; `none` removes the limit |
| `magicfs clear` | Drop filters, limit, `--when` and `--unseen`, keep the ordering |
| `magicfs refresh` | Pick up changes in the source directory |
| `magicfs status` / `list` | Inspect views |
| `magicfs close [--all]` | Remove views (links only) |
| `magicfs clean [--yes]` | Remove every view and leftover; without `--yes`, just report |
| `magicfs exec CMD...` | Run CMD with the ordered files as arguments |
| `magicfs paths [-0]` | Print the ordered real paths |
| `magicfs which NAME` | Real path behind a view entry |
| `magicfs sessions [-w WINDOW]` | List the bursts of work, for `-w @N` |
| `magicfs seen [FILE...]` | Mark files seen; with none, report the count |
| `magicfs unsee --last\|--all\|FILE...` | Make files count as new again |
| `magicfs shell-init SHELL` | Emit the auto-cd wrapper |
| `magicfs demo` | Create a throwaway directory of sample files |

There is no `shuffle` subcommand: `magicfs -s random` *is* the shuffle, and
running it again re-rolls. Every other rebuild (`refresh`, `filter`, ...) keeps
the seed, so a shuffle survives adding a file to the directory.

`magicfs close` works both from inside the view and from the directory it
presents — which is where you are after leaving one. From there it closes
every view of that directory. `magicfs clean --yes` removes the lot.

### Options

`-s/--sort` `-r/--reverse` `-f/--filter` `-x/--exclude` `-n/--limit` `-u/--unseen`
`-w/--when` `--created` `--latest` `--oldest`
`-R/--recursive` `--dirs include|exclude|only` `--name-format` `--pad`
`--case-sensitive` `--out` `--new` `--no-cd` `--shell` `--dry-run` `--real`
`--links` `-y/--yes`

Filter patterns accept a bare extension (`png`), a class (`images`, `raw`,
`video`, `audio`, `docs`, `archives`), or a glob (`'IMG_*'`, `'2024/*'`).
Matching is case-insensitive by default, so `png` catches `.PNG`.

`--recursive` flattens a whole tree into one ordered directory — useful for
`~/photos/2024/**` scattered across subdirectories.

## Trying it out

```console
$ magicfs demo              # 10 sample files in /tmp/magicfs-demo — and cds you there
```

The samples are real PNGs, so `feh *` actually opens them. Their names, sizes
(0 – 900KB), and timestamps are deliberately scrambled against each other, so
every ordering produces a visibly different result — which is the only way to
tell a sort is doing anything:

```
name:     README.md alpha-canyon bravo-forest delta-river img10 img2 ...
natural:  README.md alpha-canyon bravo-forest delta-river img2 img10 ...
size:     zulu-sunset mike-beach-day yankee-night img2 delta-river ...
time:     img10 alpha-canyon yankee-night notes.txt img2 delta-river ...
```

It includes `img2.png`/`img10.png` to show natural sort, a filename with
spaces, an empty file, and two non-images so `magicfs filter images` has
something to exclude. `-n 40` makes a bigger set; re-running replaces the
directory, and it refuses to touch one it didn't create.

## Getting into the view

A process cannot change its parent's working directory — the cwd is per-process
state inherited at `fork()`, and there is no syscall to reassign another
process's. So `magicfs` cannot `cd` your shell, and neither can any other
program.

It still gets you there, by picking the best of three options automatically.

**1. The shell wrapper**, if you installed one — the usual approach, and what
`zoxide`, `direnv`, and `autojump` all do. A function around the binary
performs the `cd` on its behalf, so you move with no nesting and `cd -` goes
back:

```sh
# bash / zsh                             # tcsh
magicfs shell-init bash >> ~/.bashrc     magicfs shell-init tcsh >> ~/.cshrc
```

The wrapper passes a scratch-file path in `MAGICFS_CD_FILE`; magicfs writes the
view path there and the shell reads it back and cds. Only view-creating
commands write it — `list`, `paths`, `exec`, and `which` never move you, and
`close` writes the *source* directory so you land somewhere that still exists.
`magicfs -s random smplayer *` writes it too, so when the player exits you are
standing where it ran.

**2. A subshell**, when there's no wrapper and you're at a terminal. magicfs
starts a *new* shell already inside the view; `exit` returns you, because your
original shell never moved. This is the default, so nothing needs installing —
the cost is that each view you open this way is one more `exit` to unwind,
which is the reason to install the wrapper.

**3. Nothing at all**, when stdout isn't a terminal — so command substitution
still behaves, and scripts get a plain path and no surprise subshell:

```sh
cd "$(magicfs ~/photos -s time)"     # bash/zsh
cd `magicfs ~/photos -s time`        # tcsh
```

`--no-cd` (or `MAGICFS_NO_CD=1`) turns the move off everywhere and just prints
the path; `--shell` forces the subshell even when output is redirected.

After landing, magicfs lists the directory for you — the resulting order *is*
the answer, and making you type `ls` to see it wastes the round trip. Set
`MAGICFS_LS` to change the command (`MAGICFS_LS='eza -l'`) or to an empty
string to turn it off. Long listings scroll rather than being truncated, the
same as `ls`. Skipped whenever output is captured.

Closing the view you're standing in is the one case that can't be tidy: with a
wrapper you're returned to the source directory, and without one magicfs leaves
the emptied view directory in place and tells you to `exit`, because deleting a
shell's cwd makes every later command fail on `getcwd`.

## Notes

- Ordering is deterministic: the same spec over an unchanged directory always
  produces byte-identical names.
- A shuffle is derived from a stored seed and each file's path, so `refresh`
  after adding a photo leaves the existing order intact instead of scrambling
  it. Only another `magicfs -s random` re-rolls.
- Dotfiles are skipped — `*` never matches them anyway.
- magicfs refuses to manage a directory containing real files, and refuses to
  build a view of a view.

## Build

```sh
cargo build --release      # target/release/magicfs
cargo test                 # 175 tests
```
