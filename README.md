# magicfs

Present a directory in whatever order you want, so that `*` expands to it.

```console
$ cd ~/photos
$ magicfs -s time                # newest-first — and it puts you in the view
$ feh *                          # opens newest-first
$ magicfs -s random              # reshuffle
$ feh *                          # now random
$ magicfs filter png             # restrict to PNGs
$ feh *                          # only PNGs, still shuffled
$ magicfs close                  # back to ~/photos, real names
```

There is nothing to install or configure first: `magicfs` on a plain directory
opens a view of it *and* moves you there. See
[Getting into the view](#getting-into-the-view) for how, and how to get back.

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

A view is a plain directory of symlinks under `$XDG_RUNTIME_DIR/magicfs/`. No
mount, no daemon, no root, nothing to leak — and because the kernel resolves a
symlink once, reads afterwards run at full native speed. Reconfiguring only
touches the links that actually moved (20,000 files: ~70ms to build, ~150ms to
reshuffle).

Views never modify or delete your real files. `magicfs close` removes links
only.

## Commands

| Command | Effect |
| --- | --- |
| `magicfs [DIR] [OPTS]` | Open a view of `DIR` (default: the current directory), or reconfigure the one you're in |
| `magicfs sort KEY` | `name`, `natural`, `time`, `ctime`, `atime`, `size`, `ext`, `random` |
| `magicfs reverse` | Flip the current order |
| `magicfs filter PAT...` | Restrict the view; no args clears |
| `magicfs exclude PAT...` | Drop matching entries |
| `magicfs limit N` | Keep the first N; `none` removes the limit |
| `magicfs clear` | Drop filters and limit, keep the ordering |
| `magicfs refresh` | Pick up changes in the source directory |
| `magicfs status` / `list` | Inspect views |
| `magicfs close [--all]` | Remove views (links only) |
| `magicfs exec CMD...` | Run CMD with the ordered files as arguments |
| `magicfs paths [-0]` | Print the ordered real paths |
| `magicfs which NAME` | Real path behind a view entry |
| `magicfs shell-init SHELL` | Emit the auto-cd wrapper |
| `magicfs demo` | Create a throwaway directory of sample files |

There is no `shuffle` subcommand: `magicfs -s random` *is* the shuffle, and
running it again re-rolls. Every other rebuild (`refresh`, `filter`, ...) keeps
the seed, so a shuffle survives adding a file to the directory.

`magicfs close` works both from inside the view and from the directory it
presents — which is where you are after leaving one.

### Options

`-s/--sort` `-r/--reverse` `-f/--filter` `-x/--exclude` `-n/--limit`
`-R/--recursive` `--dirs include|exclude|only` `--name-format` `--pad`
`--case-sensitive` `--out` `--shell` `--no-shell`

Filter patterns accept a bare extension (`png`), a class (`images`, `raw`,
`video`, `audio`, `docs`, `archives`), or a glob (`'IMG_*'`, `'2024/*'`).
Matching is case-insensitive by default, so `png` catches `.PNG`.

`--recursive` flattens a whole tree into one ordered directory — useful for
`~/photos/2024/**` scattered across subdirectories.

## Trying it out

```console
$ magicfs demo              # /tmp/magicfs-demo, 10 sample files
$ magicfs demo --open       # ...and drop straight into a view of it
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

**2. A subshell**, when there's no wrapper and you're at a terminal. magicfs
starts a *new* shell already inside the view; `exit` returns you, because your
original shell never moved. This is the default, so nothing needs installing.

**3. Nothing at all**, when stdout isn't a terminal — so command substitution
still behaves, and scripts get a plain path and no surprise subshell:

```sh
cd "$(magicfs ~/photos -s time)"     # bash/zsh
cd `magicfs ~/photos -s time`        # tcsh
```

`--shell` forces the subshell even when output is redirected; `--no-shell` (or
`MAGICFS_NO_SHELL=1`) suppresses it and just prints the path.

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
cargo test                 # 80 tests
```
