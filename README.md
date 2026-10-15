# extmv

[![CI](https://github.com/mikemadden42/extmv/actions/workflows/ci.yml/badge.svg)](https://github.com/mikemadden42/extmv/actions/workflows/ci.yml)

Tidy a directory by moving each regular file into a subdirectory named after
its extension:

```
./report.pdf   ->  Documents/pdf/report.pdf
./notes.md     ->  Documents/md/notes.md
./photo.jpg    ->  Documents/jpg/photo.jpg
```

Rust port of the C `vacuum` tool. It is non-recursive and never overwrites.
The full behavior contract is in [docs/spec.md](docs/spec.md).

## Usage

```
extmv [source_dir] [dest_base]
extmv -h | --help
```

`source_dir` defaults to `.`, `dest_base` to `Documents`.

- Hidden files, symlinks, directories and other non-regular files are skipped.
- The extension is whatever follows the last `.` (`archive.tar.gz` → `gz`),
  case preserved. Names with no `.` or a trailing `.` are skipped.
- If the destination already exists the file is left alone and reported.
- Moves across filesystems copy to a hidden `.extmv-*.tmp` file, fsync it,
  rename it into place, fsync the directory, then delete the source, so an
  interrupted move never leaves a truncated file under the real name. A
  leftover `.extmv-*.tmp` from an interrupted run is safe to delete.
- The cross-filesystem copy keeps the rwx permission bits (exactly, regardless
  of umask) and timestamps; setuid/setgid/sticky, ownership, xattrs and ACLs
  are not preserved.
- Exit status is 0 on success, 1 if anything failed.

Differences from the C version: files are processed in sorted order, empty
path arguments are rejected, `--` ends options, and the move itself is
no-clobber (`renameat2(RENAME_NOREPLACE)` on Linux, `renamex_np(RENAME_EXCL)`
on macOS, `link` + `unlink` where those are unsupported), so a file that
appears at the destination mid-run is never overwritten. On a filesystem that
supports neither, the move fails with an error instead.

## Build and test

```
cargo build --release
cargo test                         # unit tests + tests/run.sh
./tests/run.sh target/release/extmv
```
