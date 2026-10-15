#!/usr/bin/env bash
#
# Functional tests for extmv. Builds nothing itself; point it at a binary via
# $EXTMV or the first argument, otherwise it defaults to ../target/release/extmv.
#
# Usage: tests/run.sh [path-to-extmv]

set -u

script_dir=$(cd "$(dirname "$0")" && pwd)
EXTMV=${1:-${EXTMV:-"$script_dir/../target/release/extmv"}}

if [[ ! -x "$EXTMV" ]]; then
    echo "error: extmv binary not found at '$EXTMV'" >&2
    echo "build it first (cargo build --release) or pass a path" >&2
    exit 2
fi

# Resolve to an absolute path: the tests cd into temp dirs, so a relative
# binary path would no longer be found.
EXTMV="$(cd "$(dirname "$EXTMV")" && pwd)/$(basename "$EXTMV")"

pass=0
fail=0

ok() { printf 'ok   - %s\n' "$1"; pass=$((pass + 1)); }
no() { printf 'FAIL - %s\n' "$1"; fail=$((fail + 1)); }
skip() { printf 'skip - %s\n' "$1"; }

# Portable stat helpers: GNU coreutils uses -c, BSD/macOS uses -f.
if stat -c '%d' . >/dev/null 2>&1; then
    stat_dev()  { stat -c '%d' "$1" 2>/dev/null; }   # filesystem device id
    stat_mode() { stat -c '%a' "$1" 2>/dev/null; }   # permission bits (octal)
else
    stat_dev()  { stat -f '%d' "$1" 2>/dev/null; }
    stat_mode() { stat -f '%Lp' "$1" 2>/dev/null; }
fi

# assert that a file exists
assert_file() {
    if [[ -f "$1" ]]; then ok "$2"; else no "$2 (missing: $1)"; fi
}

# assert that a path does NOT exist
assert_absent() {
    if [[ ! -e "$1" ]]; then ok "$2"; else no "$2 (present: $1)"; fi
}

# assert that a path is a symlink (regardless of whether its target exists)
assert_symlink() {
    if [[ -L "$1" ]]; then ok "$2"; else no "$2 (not a symlink: $1)"; fi
}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# --- core: bucketing, dotfile / extensionless / symlink skipping ---
src="$work/core"
mkdir -p "$src"
( cd "$src"
  touch report.pdf notes.md a.txt b.txt archive.tar.gz noext .hidden.txt
  ln -s a.txt link.txt
  "$EXTMV" . Documents >/dev/null
)
assert_file   "$src/Documents/pdf/report.pdf"   "pdf bucketed"
assert_file   "$src/Documents/md/notes.md"      "md bucketed"
assert_file   "$src/Documents/txt/a.txt"        "txt bucketed (a)"
assert_file   "$src/Documents/txt/b.txt"        "txt bucketed (b)"
assert_file   "$src/Documents/gz/archive.tar.gz" "multi-dot uses last ext"
assert_absent "$src/Documents/noext"            "extensionless skipped"
assert_file   "$src/noext"                       "extensionless left in place"
assert_file   "$src/.hidden.txt"                 "dotfile left in place"
assert_symlink "$src/link.txt"                   "symlink left in place"
assert_absent "$src/Documents/txt/link.txt"     "symlink not moved"

# --- already-exists path: file is left in source, message printed ---
src="$work/exists"
mkdir -p "$src/Documents/txt"
( cd "$src"
  touch a.txt
  echo keep > Documents/txt/a.txt
  out=$("$EXTMV" . Documents)
  echo "$out" | grep -q "already exists" || exit 3
)
if [[ $? -eq 0 ]]; then ok "already-exists prints message"; else no "already-exists prints message"; fi
assert_file "$src/a.txt"                  "already-exists leaves source file"
[[ "$(cat "$src/Documents/txt/a.txt")" == "keep" ]] \
    && ok "already-exists does not overwrite" \
    || no "already-exists does not overwrite"

# --- custom dest_base, with a trailing slash ---
# (// and / are the same path to the kernel, so verify via the printed message)
src="$work/dest"
mkdir -p "$src"
out=$( cd "$src"; touch x.log; "$EXTMV" . sorted/ )
assert_file "$src/sorted/log/x.log"  "custom dest_base honored"
if [[ "$out" != *"//"* ]]; then
    ok "trailing slash not duplicated in output"
else
    no "trailing slash not duplicated in output (got: $out)"
fi

# --- cross-filesystem move (EXDEV fallback) ---
# Needs a writable directory on a different filesystem; /dev/shm is the usual
# candidate on Linux. Skipped when no second filesystem is available.
otherfs=""
for cand in /dev/shm /run/user/"$(id -u)" /Volumes/RAMDisk "${TMPDIR:-}"; do
    [[ -n "$cand" ]] || continue
    if [[ -d "$cand" && -w "$cand" && "$(stat_dev "$cand")" != "$(stat_dev "$work")" ]]; then
        otherfs="$cand"; break
    fi
done
if [[ -n "$otherfs" ]]; then
    src="$work/xdev"
    dst=$(mktemp -d "$otherfs/extmv.XXXXXX")
    mkdir -p "$src"
    ( cd "$src"; printf 'payload\n' > move.bin; chmod 640 move.bin
      "$EXTMV" . "$dst" >/dev/null )
    assert_file "$dst/bin/move.bin"          "EXDEV: file copied to other fs"
    assert_absent "$src/move.bin"            "EXDEV: source removed"
    [[ "$(cat "$dst/bin/move.bin" 2>/dev/null)" == "payload" ]] \
        && ok "EXDEV: content intact" || no "EXDEV: content intact"
    mode=$(stat_mode "$dst/bin/move.bin")
    if [[ "$mode" =~ ^[0-7]+$ ]] && (( 8#$mode == 8#640 )); then
        ok "EXDEV: permissions preserved"
    else
        no "EXDEV: permissions preserved (got: $mode)"
    fi
    rm -rf "$dst"
else
    skip "EXDEV cross-filesystem move (no second filesystem available)"
fi

# --- CLI behavior ---
"$EXTMV" --help >/dev/null 2>&1 && ok "--help exits 0" || no "--help exits 0"
"$EXTMV" -h     >/dev/null 2>&1 && ok "-h exits 0"     || no "-h exits 0"
"$EXTMV" --bogus >/dev/null 2>&1 && no "unknown option fails" || ok "unknown option fails"
"$EXTMV" a b c   >/dev/null 2>&1 && no "too many args fails"  || ok "too many args fails"
"$EXTMV" /no/such/dir >/dev/null 2>&1 && no "missing dir fails" || ok "missing dir fails"


# ---------------------------------------------------------------------------
# Port-specific cases (spec §7 "additional cases"; not in the C suite)
# ---------------------------------------------------------------------------

# --- trailing-dot names have no usable extension ---
src="$work/trailing-dot"
mkdir -p "$src"
( cd "$src"; touch foo. a..; "$EXTMV" >/dev/null )
assert_file   "$src/foo."       "foo. left in place"
assert_file   "$src/a.."        "a.. left in place"
assert_absent "$src/Documents"  "trailing-dot names create no buckets"

# --- extension case is preserved ---
src="$work/case"
mkdir -p "$src"
( cd "$src"; touch A.JPG b.jpg; "$EXTMV" >/dev/null )
assert_file "$src/Documents/JPG/A.JPG" "uppercase extension keeps its own bucket"
assert_file "$src/Documents/jpg/b.jpg" "lowercase extension keeps its own bucket"

# --- subdirectories are skipped ---
src="$work/subdir"
mkdir -p "$src/dir.d"
( cd "$src"; "$EXTMV" >/dev/null )
[[ -d "$src/dir.d" ]] && ok "subdirectory left in place" || no "subdirectory left in place"
assert_absent "$src/Documents/d" "subdirectory not bucketed"

# --- non-UTF-8 file name ---
# Some filesystems (APFS) refuse such names outright; nothing to test there.
src="$work/bytes"
mkdir -p "$src"
if touch "$src/"$'\xff.txt' 2>/dev/null; then
    ( cd "$src"; "$EXTMV" >/dev/null )
    assert_file   "$src/Documents/txt/"$'\xff.txt' "non-UTF-8 name moved"
    assert_absent "$src/"$'\xff.txt'              "non-UTF-8 source removed"
else
    skip "non-UTF-8 file name (filesystem rejects it)"
fi

# --- bucket path blocked by a regular file: error, but the run continues ---
src="$work/blocked"
mkdir -p "$src/Documents"
( cd "$src"; touch Documents/txt a.txt b.md
  "$EXTMV" >/dev/null 2>err; echo $? > rc )
[[ "$(cat "$src/rc")" == 1 ]] && ok "blocked bucket exits 1" || no "blocked bucket exits 1"
grep -qE "^extmv: cannot (create|access) 'Documents/txt" "$src/err" \
    && ok "blocked bucket reported on stderr" || no "blocked bucket reported on stderr"
assert_file "$src/a.txt"             "blocked file left in place"
assert_file "$src/Documents/md/b.md" "other files still moved"

# --- read-only source dir: rename fails, every file is reported ---
if [[ $(id -u) -ne 0 ]]; then
    src="$work/readonly"
    mkdir -p "$src/in"
    touch "$src/in/a.txt" "$src/in/b.md"
    chmod 555 "$src/in"
    ( cd "$src"; "$EXTMV" in out >/dev/null 2>err; echo $? > rc )
    chmod 755 "$src/in"
    [[ "$(cat "$src/rc")" == 1 ]] && ok "read-only source exits 1" || no "read-only source exits 1"
    [[ "$(grep -c "^extmv: cannot move 'in/" "$src/err")" == 2 ]] \
        && ok "read-only source reports each file" || no "read-only source reports each file"
    assert_file "$src/in/a.txt" "read-only source keeps files"
else
    skip "read-only source dir (running as root)"
fi

# --- stdout write errors are detected ---
if [[ -w /dev/full ]]; then
    src="$work/full"
    mkdir -p "$src"
    ( cd "$src"; touch a.txt; "$EXTMV" >/dev/full 2>err; echo $? > rc )
    [[ "$(cat "$src/rc")" == 1 ]] && ok "/dev/full exits 1" || no "/dev/full exits 1"
    grep -q "^extmv: error writing to stdout" "$src/err" \
        && ok "/dev/full reported" || no "/dev/full reported"
    assert_file "$src/Documents/txt/a.txt" "/dev/full still moves files"
else
    skip "stdout write error (/dev/full not available)"
fi

# --- empty path arguments are usage errors, nothing is touched ---
src="$work/empty"
mkdir -p "$src"
touch "$src/a.txt"
( cd "$src"; "$EXTMV" . "" >out 2>err ) && no "empty dest_base fails" || ok "empty dest_base fails"
grep -q "^Usage:" "$src/err" && ok "empty dest_base prints usage" || no "empty dest_base prints usage"
[[ ! -s "$src/out" ]] && ok "empty dest_base moves nothing" || no "empty dest_base moves nothing"
"$EXTMV" "" x >/dev/null 2>&1 && no "empty source_dir fails" || ok "empty source_dir fails"
assert_file "$src/a.txt" "empty args leave files in place"

# --- `--` ends options (port extension) ---
src="$work/dashdash"
mkdir -p "$src/-dir"
( cd "$src"; touch -- -dir/a.txt; "$EXTMV" -- -dir out >/dev/null )
assert_file "$src/out/txt/a.txt" "-- allows a dash-leading source_dir"

# --- lone '-' is a positional ---
src="$work/dash"
mkdir -p "$src/-"
( cd "$src"; touch -- -/a.txt; "$EXTMV" - out >/dev/null )
assert_file "$src/out/txt/a.txt" "lone - is a directory name"

# --- default run with an existing ./Documents ---
src="$work/default"
mkdir -p "$src/Documents"
( cd "$src"; touch a.txt; "$EXTMV" >/dev/null )
assert_file   "$src/Documents/txt/a.txt"     "default run uses ./Documents"
assert_absent "$src/Documents/Documents"     "Documents not moved into itself"

# --- output format ---
src="$work/format"
mkdir -p "$src/Documents/txt"
( cd "$src"; touch a.txt b.txt Documents/txt/b.txt; "$EXTMV" >out )
[[ "$(cat "$src/out")" == $'Moved a.txt to Documents/txt\nFile b.txt already exists in Documents/txt' ]] \
    && ok "stdout messages exact and sorted" || no "stdout messages exact and sorted (got: $(cat "$src/out"))"

echo
echo "passed: $pass, failed: $fail"
[[ $fail -eq 0 ]]
