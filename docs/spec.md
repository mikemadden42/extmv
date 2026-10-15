# extmv — Behavioral Specification

Version: 1.1 (2026-10-15). 1.0 was derived from `vacuum.c` (C17, repo
`mikemadden42/vacuum`) at commit `1988a1a`. 1.1 records the choices this Rust
implementation made where 1.0 left room (`--`, sort order, no-clobber moves,
SIGPIPE), the hardened cross-filesystem move (§3.6), and the test suite as it
now stands (§7).

`extmv` is the new name for this tool. The C reference implementation is still
called `vacuum`; mentions of `vacuum` or "the C version" below refer to it.
Everything else describes the `extmv` contract.

The spec says what the tool does, byte-for-byte where it matters, and flags
the places where the C version has sharp edges that a port should fix rather
than copy. Places where extmv goes beyond or deliberately departs from 1.0 are
marked **(extmv)**.

Keywords: **MUST** = required for compatibility; **SHOULD** = recommended
improvement over the C version; **MAY** = optional.

---

## 1. Purpose

`extmv` tidies one directory by moving each regular file into a subdirectory
named after its extension:

```
./report.pdf   ->  Documents/pdf/report.pdf
./notes.md     ->  Documents/md/notes.md
./photo.jpg    ->  Documents/jpg/photo.jpg
```

It is non-recursive, never overwrites, and never deletes data except as the
second half of a move.

---

## 2. Command line

```
extmv [--] [source_dir] [dest_base]
extmv -h | --help
```

| Position | Name         | Default     |
| -------- | ------------ | ----------- |
| 1        | `source_dir` | `.`         |
| 2        | `dest_base`  | `Documents` |

Both are used as given (relative paths resolve against the CWD). They are
**not** canonicalized or normalized.

### 2.1 Parsing rules (MUST)

Arguments are scanned left to right; the first rule that matches wins. Rules
1–3 apply only until `--` has been seen.

1. Arg is exactly `-h` or `--help` → print usage to **stdout**, exit **0**
   immediately (no files touched).
2. Arg is exactly `--` → end of options; skip it. Every later argument is a
   positional, even `-h` or `-foo`. **(extmv)**
3. Arg starts with `-` and is longer than one character → print
   `extmv: unknown option '<arg>'` + usage to **stderr**, exit **1**.
4. Two positionals already collected → print `extmv: too many arguments`
   + usage to **stderr**, exit **1**.
5. Arg is empty → print `extmv: empty path argument` + usage to **stderr**,
   exit **1** (see §3.4).
6. Otherwise it is a positional. (A lone `-` is a positional, not stdin.)

Because scanning stops at the first decisive arg, `extmv a b c -h` fails
("too many arguments") while `extmv -h a b c` prints help.

C quirk: `vacuum` has no `--`; it is rejected as an unknown option, and a
directory named `-foo` must be passed as `./-foo`. 1.0 allowed a port to add
`--`; extmv does. It doesn't change any invocation that `vacuum` accepts.

### 2.2 Usage text (MUST, exact)

```
Usage: extmv [source_dir] [dest_base]

Organize regular files in source_dir into
<dest_base>/<extension>/ subdirectories.

  source_dir   directory to scan        (default ".")
  dest_base    base for extension dirs  (default "Documents")

Options:
  -h, --help   show this help and exit
```

(Program name is the literal `extmv`, not `argv[0]`.)

---

## 3. Algorithm

### 3.1 Snapshot (MUST)

1. Open `source_dir` and read **all** entry names into memory, then close it.
   - Open failure → stderr `extmv: cannot open '<source_dir>': <reason>`,
     exit 1, nothing moved.
   - Error partway through reading → stderr
     `extmv: error reading '<source_dir>': <reason>`, exit 1, **nothing
     moved** (the partial listing is discarded).
2. Only after the snapshot is complete does any move happen. Moving files
   while iterating a live directory handle is forbidden.

Order: the C version processes entries in raw `readdir` order (unspecified,
filesystem dependent). extmv sorts names bytewise ascending, so output is
deterministic. Nothing else depends on the order.

### 3.2 Per-entry filtering (MUST)

For each name, in order, skip it silently (no output, not an error) if:

1. **Hidden:** name begins with `.` (this also covers `.` and `..`).
2. **Not a regular file:** determined with `lstat` semantics — symlinks are
   **not followed**, so a symlink (even to a regular file) is skipped.
   Directories, FIFOs, sockets, devices are skipped. If the `lstat` call
   itself fails (e.g. the file vanished), the entry is silently skipped.
3. **No usable extension:** see §3.3.

### 3.3 Extension rule (MUST)

- The extension is the substring **after the last `.`** in the file name.
- If there is no `.`, or the `.` is the last character (`foo.`, `a..`), the
  file has no usable extension and is skipped.
- Multi-dot names use only the last component: `archive.tar.gz` → `gz`.
- Case is **preserved**: `A.JPG` → `JPG/`, `b.jpg` → `jpg/` (two buckets).
  Do not lowercase.
- The extension is used verbatim as a directory name. (It can't contain `/`
  because it comes from a single path component, and it can't be `.` or `..`
  because it contains no `.`.)

Python's `os.path.splitext`, Rust's `Path::extension`, and Go's
`filepath.Ext` all differ slightly at the edges (`foo.` and leading-dot
handling). Implement the rule above directly instead of relying on them.

### 3.4 Paths

```
src_path  = join(source_dir, name)
dest_dir  = join(dest_base, ext)
dest_path = join(dest_dir, name)
```

`join(a, b)`: if `a` ends in `/`, return `a + b`; otherwise `a + "/" + b`.
Only one trailing slash is absorbed (`sorted//` → `sorted//log`). `dest_dir`
appears in output messages exactly as built here.

> ⚠ C foot-gun: `join("", b)` yields `"/" + b`, so `vacuum . ""` writes into
> `/pdf/`, `/txt/`… at the filesystem root. A port **MUST** reject empty
> `source_dir`/`dest_base` arguments (usage error, exit 1; §2.1 rule 5)
> instead of copying this behavior.

### 3.5 Per-file action

For each surviving entry, in this order:

1. **Create `dest_dir`** recursively (`mkdir -p` semantics, mode `0777`
   before umask; existing directories are fine).
   On failure → stderr `extmv: cannot create '<dest_dir>': <reason>`,
   mark run failed, continue with next file.
2. **Collision check.** If `dest_path` exists (any type, including a dangling
   symlink) → stdout `File <name> already exists in <dest_dir>`; source
   untouched; **not** an error. If checking existence fails for any reason
   other than "not found" (e.g. permission denied) → stderr
   `extmv: cannot access '<dest_path>': <reason>`, mark failed, continue.
3. **Move** `src_path` → `dest_path` without ever replacing `dest_path`:
   - Try an atomic **no-clobber** rename (below).
   - If it fails with **EXDEV** (cross-filesystem), fall back to
     copy-then-delete (§3.6).
   - If the move fails because `dest_path` now exists (it appeared after
     step 2) → treat it exactly as the step 2 collision: stdout
     `File <name> already exists in <dest_dir>`, not an error.
   - Success → stdout `Moved <name> to <dest_dir>`.
   - Any other failure → stderr
     `extmv: cannot move '<src_path>' to '<dest_path>': <reason>`,
     mark failed, continue.

Note that step 1 runs before step 2, so the bucket directory is created even
when the file then turns out to already exist (harmless, since the directory
must already exist for the collision to happen).

**No-clobber rename.** The C version checks (step 2) and then calls
`rename(2)`, so a file created at `dest_path` in between is **silently
overwritten**. 1.0 said a port SHOULD close that race; extmv does:

- Linux: `renameat2(..., RENAME_NOREPLACE)` (raw syscall, so it doesn't need
  a recent libc).
- macOS: `renamex_np(..., RENAME_EXCL)`.
- If that call reports it isn't supported (`EINVAL`, `ENOSYS`, `ENOTSUP`,
  `EOPNOTSUPP`), or on other systems: `link(src, dst)` then `unlink(src)`.
  `link` fails with `EEXIST` atomically. If the `unlink` fails, the new link
  is removed again (same inode, nothing lost) and the move fails.
- A filesystem that supports neither (no hard links either) makes the move
  **fail with an error**. extmv never falls back to a plain `rename`.

Step 2 stays: it is what produces the message in the common case without
attempting a move.

### 3.6 Cross-filesystem fallback (MUST)

When the rename fails with EXDEV, extmv copies through a hidden temporary file
so that `dest_path` only ever appears complete and durable. 1.0 described the
C version, which wrote straight to `dest_path` and didn't fsync; an
interrupted copy there leaves a truncated file under the real name that a
rerun reports as "already exists" with exit 0. **(extmv)**

1. **Open the source** read-only with `O_NOFOLLOW | O_NONBLOCK`, `fstat` it,
   and require a regular file. The entry passed §3.2 when it was checked but
   may have been swapped since: a symlink is not followed and a FIFO doesn't
   block. Either one fails the move with reason `source changed during move`.
2. **Create a temporary file** in `dest_dir` exclusively (`O_CREAT|O_EXCL`,
   mode `0600`), named `.extmv-<pid>-<n>.tmp`, where `n` counts up past names
   left by earlier runs. The name doesn't include the file's own name, which
   could push it past `NAME_MAX`. It is hidden, so a later run skips it.
3. **Set its mode** to the source permission bits `& 0777` with `fchmod`, so
   umask doesn't alter them. setuid/setgid/sticky are dropped on purpose.
   **(extmv)** In the C version umask still applies, so a `0666` source can
   arrive as `0644`; extmv follows `mv` instead.
4. **Copy all bytes.**
5. **Preserve atime/mtime**, best effort: failing to set them doesn't fail
   the move.
6. **fsync** the temporary file, then **close** it; a close error counts as a
   failure.
7. **Rename the temporary to `dest_path`** with the no-clobber rename of §3.5.
   If `dest_path` exists, this is the collision case: the temporary is
   removed and the "already exists" line printed.
8. **fsync `dest_dir`**, so the new directory entry is durable before the
   source goes away. A filesystem that can't sync directories (`EINVAL`,
   `ENOTSUP`, …) is tolerated. Any other failure removes `dest_path` again
   and fails the move, leaving the source intact.
9. **Delete the source**, but only if `src_path` still has the device and
   inode that were copied (`lstat`). If it doesn't, keep both and fail with
   `source changed during move`. A replacement made between this check and
   the `unlink` would still be deleted; POSIX has no "unlink if inode
   matches", so the window can only be narrowed.
10. If deleting the source fails, `dest_path` is **kept** (both copies now
    exist) and the move is reported as failed.

On any failure in steps 1–7 the source is left intact, and a temporary that
was created is removed. If the process is killed mid-copy, at most a stray
`.extmv-<pid>-<n>.tmp` remains in `dest_dir`; it is safe to delete.

Not preserved: setuid/setgid/sticky, owner/group, xattrs, ACLs, hard-link
identity.

### 3.7 Finish

After all entries:

- Flush stdout. If flushing or any earlier stdout write failed (e.g.
  `extmv > /dev/full`) → stderr `extmv: error writing to stdout`, mark failed.
- Exit **1** if any failure was marked, else **0**.

**SIGPIPE (extmv).** The C version is killed silently by SIGPIPE on
`vacuum | head`, which can stop it mid-run with files left unsorted. extmv
ignores SIGPIPE (the Rust default): after the first failed write it stops
writing to stdout, finishes processing every file, then reports
`error writing to stdout` and exits 1.

Help output (`-h`/`--help`) is written without checking for errors and always
exits 0.

Out-of-memory is fatal (C aborts with exit 1); extmv leaves it to the Rust
runtime.

---

## 4. Output contract

| Stream | Text                                                        | Error? |
| ------ | ----------------------------------------------------------- | ------ |
| stdout | `Moved <name> to <dest_dir>`                                | no     |
| stdout | `File <name> already exists in <dest_dir>`                  | no     |
| stderr | `extmv: cannot open '<source_dir>': <reason>`               | fatal  |
| stderr | `extmv: error reading '<source_dir>': <reason>`             | fatal  |
| stderr | `extmv: cannot create '<dest_dir>': <reason>`               | yes    |
| stderr | `extmv: cannot access '<dest_path>': <reason>`              | yes    |
| stderr | `extmv: cannot move '<src_path>' to '<dest_path>': <reason>` | yes   |
| stderr | `extmv: error writing to stdout`                            | yes    |
| stderr | `extmv: unknown option '<arg>'` + usage                     | fatal  |
| stderr | `extmv: too many arguments` + usage                         | fatal  |
| stderr | `extmv: empty path argument` + usage **(extmv)**            | fatal  |

Each line ends with `\n`. `<reason>` is the OS error string (`strerror`; extmv
strips Rust's ` (os error N)` suffix), or `source changed during move` (§3.6).
It doesn't have to match byte-for-byte across languages, and tests should only
match the prefix. Names, paths and arguments are printed raw (no quoting or
escaping).

stdout is buffered; stderr is not. When both go to a terminal, an error line
can therefore appear before "Moved" lines for files processed earlier.

Exit codes: `0` success, `1` any failure. (The test script uses `2` for
"binary not found"; that's the test harness, not the tool.)

---

## 5. Things a port must get right (where C was saving you, or not)

1. **Filenames are bytes, not text.** POSIX names can be invalid UTF-8. The C
   version handles them for free. In Rust use `OsStr`/`OsString` (not
   `String`), in Go `string` is fine (it's bytes), in Python use `os.fsencode`
   / surrogateescape (the default `str` APIs already do this), in Zig `[]u8`,
   in Swift avoid `String` round-trips. Never panic or skip on a non-UTF-8
   name; split on the byte `0x2E` (`.`).
2. **Don't follow symlinks** in the regular-file test (`symlink_metadata` /
   `os.lstat` / `Lstat`), not `metadata` / `stat`.
3. **Do follow** a symlink if it is `source_dir` itself or a component of
   `dest_base`, including an existing `dest_dir` (normal path resolution,
   matching C `opendir`/`mkdir`). In a shared or world-writable directory this
   lets another user redirect files by planting a symlink at a bucket path.
4. **No overwrite, ever.** Many standard-library "move" helpers overwrite
   (Rust `fs::rename`, Go `os.Rename`, Python `os.rename` on POSIX,
   `shutil.move`). See §3.5 for the no-clobber rename.
5. **Detect EXDEV specifically** (Rust `ErrorKind::CrossesDevices`, Go
   `errors.Is(err, syscall.EXDEV)`, Python `e.errno == errno.EXDEV`). Don't
   fall back to copying on other errors.
6. **One bad file must not stop the run.** Collect a failure flag, keep going.
7. **Reject empty path arguments** (see §3.4).
8. **Check stdout errors at exit.** Many runtimes swallow them (Rust
   `println!` panics on EPIPE instead; Go's `fmt.Println` ignores errors).
   Use an explicit buffered writer and check flush. Decide on SIGPIPE; extmv's
   choice is in §3.7.
9. **Snapshot first.** Collect the listing into a `Vec`/slice/list before
   moving anything, and treat a mid-listing read error as fatal.
10. **Never write a partial file under the real name** in the cross-filesystem
    fallback (§3.6). **(extmv)**

---

## 6. Explicit non-goals

- No recursion into subdirectories.
- No dry-run, verbose/quiet, config file, or extension mapping (e.g.
  `jpeg` → `jpg`). These are fine to add as extensions, but the defaults above
  must stay the default behavior.
- No Windows semantics are specified (the C version is POSIX-only; extmv
  builds and is tested on Linux and macOS only).

---

## 7. Conformance tests

`tests/run.sh <binary>` is a black-box suite. Point it at any implementation
(`./tests/run.sh target/release/extmv`); with no argument it uses
`$EXTMV` or `../target/release/extmv`. `cargo test` runs it against the
freshly built binary (`tests/conformance.rs`), alongside unit tests for the
parts the shell can't reach (§7.3).

### 7.1 Cases from the C suite

`vacuum`'s `tests/run.sh`, ported with only the binary name changed:

| Case                                                                | Expected                                              |
| ------------------------------------------------------------------- | ----------------------------------------------------- |
| `report.pdf notes.md a.txt b.txt` in `.`                            | bucketed into `Documents/{pdf,md,txt}/`               |
| `archive.tar.gz`                                                    | `Documents/gz/archive.tar.gz`                         |
| `noext`                                                             | left in place, no `Documents/noext`                   |
| `.hidden.txt`                                                       | left in place                                         |
| `link.txt -> a.txt` (symlink)                                       | left in place, still a symlink                        |
| `a.txt` with existing `Documents/txt/a.txt` containing `keep`       | stdout has `already exists`; both files unchanged     |
| `extmv . sorted/` with `x.log`                                      | `sorted/log/x.log`; output contains no `//`           |
| dest on another filesystem (`/dev/shm`), `move.bin` mode `640`      | moved, source gone, contents intact, mode `640`       |
| `--help`, `-h`                                                      | exit 0                                                |
| `--bogus`                                                           | exit ≠ 0                                              |
| `a b c`                                                             | exit ≠ 0                                              |
| `/no/such/dir`                                                      | exit ≠ 0                                              |

### 7.2 Cases added for extmv

| Case                                                     | Expected                                                   |
| -------------------------------------------------------- | ---------------------------------------------------------- |
| `foo.` and `a..`                                         | skipped, no buckets created                                |
| `A.JPG` + `b.jpg`                                        | `JPG/A.JPG`, `jpg/b.jpg` (case preserved)                  |
| Subdirectory `dir.d/` in source                          | skipped                                                    |
| Filename with invalid UTF-8 bytes, `$'\xff.txt'`         | moved to `txt/`                                            |
| `Documents/txt` is a regular file                        | exit 1, `cannot create` or `cannot access` on stderr, other files still moved |
| Read-only source dir (rename gets EACCES)                | exit 1, one `cannot move` line per file, files kept        |
| `extmv > /dev/full`                                      | exit 1, `error writing to stdout`, files still moved       |
| `extmv . ""` / `extmv "" x`                              | exit 1, usage on stderr, nothing moved                     |
| `extmv -- -dir out`                                      | `-dir` used as `source_dir`                                |
| `extmv - out`                                            | lone `-` used as `source_dir`                              |
| Default run where `./Documents` already exists           | files go into it; `Documents` itself is not moved          |
| `a.txt` + `b.txt` with `Documents/txt/b.txt` existing    | stdout exactly `Moved a.txt …` then `File b.txt already exists …` (sorted) |
| Cross-fs, `0666` source under `umask 022`                | arrives as `0666` (§3.6 step 3)                            |
| Cross-fs, 512 MiB file, SIGTERM mid-copy                 | `dest_path` absent (source intact) or complete; never truncated |

The interrupt case can't fail because of timing: whenever the signal lands,
the result must be one of the two allowed states. The file is large so the
signal usually lands mid-copy; against a binary that writes straight to
`dest_path` (the C approach), the test fails.

Cases skip themselves where they can't run: the cross-filesystem cases when no
writable directory on a second filesystem is found (`/dev/shm`,
`/run/user/<uid>`, `/Volumes/RAMDisk`, `$TMPDIR`), the non-UTF-8 name on
filesystems that reject it (APFS), `/dev/full` where it doesn't exist, and
the read-only case when run as root. On macOS CI the first three skip.

### 7.3 Unit tests

Cases that need a race or a contrived file type the shell can't arrange
reliably are tested by calling the internals directly: argument parsing
(§2.1), the extension and join rules (§3.3, §3.4), the link-then-unlink
fallback (moves; refuses an existing destination), and the copy (exact mode
despite umask, refuses a symlink or FIFO source without blocking, leaves no
temporary behind, refuses an existing destination and cleans up, skips stale
temporary names).

Not covered by any test: a source replaced mid-copy (§3.6 step 9), a copy
failing partway (e.g. `ENOSPC`), a failed directory fsync, and the
`cannot access` branch, which in practice only a race can reach.
