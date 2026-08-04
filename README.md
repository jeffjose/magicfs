# magicfs

Present a directory in whatever order you want, so that `*` expands to it.

```console
$ cd ~/photos
$ cd "$(magicfs . -s time)"      # newest-first view
$ feh *                          # opens newest-first
$ magicfs shuffle                # reshuffle in place
$ feh *                          # now random
$ magicfs filter png             # restrict to PNGs
$ feh *                          # only PNGs, still shuffled
```

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
| `magicfs [DIR] [OPTS]` | Create a view (or reconfigure the one you're in) and print its path |
| `magicfs sort KEY` | `name`, `natural`, `time`, `ctime`, `atime`, `size`, `ext`, `random` |
| `magicfs shuffle` | Re-roll the random order |
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

### Options

`-s/--sort` `-r/--reverse` `-f/--filter` `-x/--exclude` `-n/--limit`
`-R/--recursive` `--dirs include|exclude|only` `--name-format` `--pad`
`--case-sensitive` `--out`

Filter patterns accept a bare extension (`png`), a class (`images`, `raw`,
`video`, `audio`, `docs`, `archives`), or a glob (`'IMG_*'`, `'2024/*'`).
Matching is case-insensitive by default, so `png` catches `.PNG`.

`--recursive` flattens a whole tree into one ordered directory — useful for
`~/photos/2024/**` scattered across subdirectories.

## Shell integration

Optional; it makes `magicfs` cd into the view for you.

```sh
# bash / zsh                             # tcsh
magicfs shell-init bash >> ~/.bashrc     magicfs shell-init tcsh >> ~/.cshrc
```

Then `magicfs ~/photos -s time` drops you straight into the view. Without it,
use `cd "$(magicfs ~/photos -s time)"`.

## Notes

- Ordering is deterministic: the same spec over an unchanged directory always
  produces byte-identical names.
- A shuffle is derived from a stored seed and each file's path, so `refresh`
  after adding a photo leaves the existing order intact instead of scrambling
  it. Only `magicfs shuffle` re-rolls.
- Dotfiles are skipped — `*` never matches them anyway.
- magicfs refuses to manage a directory containing real files, and refuses to
  build a view of a view.

## Build

```sh
cargo build --release      # target/release/magicfs
cargo test                 # 61 tests
```
