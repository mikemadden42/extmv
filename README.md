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
- Moves across filesystems fall back to copy + fsync + delete, keeping
  permission bits and timestamps.
- Exit status is 0 on success, 1 if anything failed.

Differences from the C version: files are processed in sorted order, empty
path arguments are rejected, `--` ends options, and the move itself is
no-clobber (`renameat2(RENAME_NOREPLACE)` on Linux, `renamex_np(RENAME_EXCL)`
on macOS), so a file that appears at the destination mid-run is never
overwritten.

## Build and test

```
cargo build --release
cargo test                         # unit tests + tests/run.sh
./tests/run.sh target/release/extmv
```
