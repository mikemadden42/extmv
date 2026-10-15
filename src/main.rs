//! extmv: move each regular file in a directory into `<dest_base>/<ext>/`.
//!
//! Rust port of the C `vacuum`/`extmv` tool; see the behavioral spec for the
//! exact contract. Differences from the C version:
//!
//! - entries are processed in bytewise-sorted order;
//! - empty `source_dir`/`dest_base` arguments are rejected;
//! - `--` ends option parsing;
//! - the move is no-clobber (`renameat2(RENAME_NOREPLACE)` on Linux,
//!   `renamex_np(RENAME_EXCL)` on macOS, else `link` + `unlink`), closing the
//!   check-then-rename race;
//! - the cross-filesystem fallback copies via a hidden temporary file renamed
//!   into place (so an interrupted copy never leaves a truncated file under
//!   the real name), fsyncs the file and its directory before deleting the
//!   source, preserves atime/mtime, and refuses to delete a source path that
//!   no longer names the copied file;
//! - the copy gets the exact rwx mode, ignoring umask (spec §3.6 has umask
//!   apply; this follows `mv` instead).

use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, FileTimes, Metadata, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::os::fd::IntoRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "\
Usage: extmv [source_dir] [dest_base]

Organize regular files in source_dir into
<dest_base>/<extension>/ subdirectories.

  source_dir   directory to scan        (default \".\")
  dest_base    base for extension dirs  (default \"Documents\")

Options:
  -h, --help   show this help and exit
";

#[derive(Debug, PartialEq)]
enum Cli {
    Help,
    Run {
        source: OsString,
        dest_base: OsString,
    },
    /// Usage error; the message (without the `extmv: ` prefix) as raw bytes.
    Usage(Vec<u8>),
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> Cli {
    let mut positionals: Vec<OsString> = Vec::with_capacity(2);
    let mut options_done = false;

    for arg in args {
        let bytes = arg.as_bytes();
        if !options_done {
            if bytes == b"-h" || bytes == b"--help" {
                return Cli::Help;
            }
            if bytes == b"--" {
                options_done = true;
                continue;
            }
            if bytes.len() > 1 && bytes[0] == b'-' {
                return Cli::Usage([b"unknown option '", bytes, b"'"].concat());
            }
        }
        if positionals.len() == 2 {
            return Cli::Usage(b"too many arguments".to_vec());
        }
        if bytes.is_empty() {
            // join("", x) would yield "/x" and scatter files across the root.
            return Cli::Usage(b"empty path argument".to_vec());
        }
        positionals.push(arg);
    }

    let mut it = positionals.into_iter();
    Cli::Run {
        source: it.next().unwrap_or_else(|| ".".into()),
        dest_base: it.next().unwrap_or_else(|| "Documents".into()),
    }
}

/// The extension is everything after the last `.`; a trailing `.` (or no `.`
/// at all) means there is none. Case is preserved.
fn extension(name: &[u8]) -> Option<&[u8]> {
    let dot = name.iter().rposition(|&b| b == b'.')?;
    let ext = &name[dot + 1..];
    (!ext.is_empty()).then_some(ext)
}

/// `a + b`, inserting a `/` unless `a` already ends with one.
fn join(a: &[u8], b: &[u8]) -> OsString {
    let mut v = Vec::with_capacity(a.len() + 1 + b.len());
    v.extend_from_slice(a);
    if !a.ends_with(b"/") {
        v.push(b'/');
    }
    v.extend_from_slice(b);
    OsString::from_vec(v)
}

/// The OS error string, without Rust's " (os error N)" suffix.
fn reason(e: &io::Error) -> Vec<u8> {
    let s = e.to_string();
    match s.rfind(" (os error ") {
        Some(i) => s.as_bytes()[..i].to_vec(),
        None => s.into_bytes(),
    }
}

/// Write one `extmv: ...` line to stderr. Failures are ignored: there is
/// nowhere left to report them.
fn report(parts: &[&[u8]]) {
    let mut msg = b"extmv: ".to_vec();
    msg.extend(parts.concat());
    msg.push(b'\n');
    let _ = io::stderr().write_all(&msg);
}

/// Buffered stdout that remembers whether any write failed. After the first
/// failure it stops writing but the run continues, so a closed pipe never
/// leaves files half-organized.
struct Out<W: Write> {
    w: W,
    failed: bool,
}

impl<W: Write> Out<W> {
    fn line(&mut self, parts: &[&[u8]]) {
        if self.failed {
            return;
        }
        let mut msg = parts.concat();
        msg.push(b'\n');
        if self.w.write_all(&msg).is_err() {
            self.failed = true;
        }
    }

    fn finish(mut self) -> bool {
        !self.failed && self.w.flush().is_ok()
    }
}

/// Read every entry name of `dir` before anything is moved. Any error is
/// reported and fatal.
fn snapshot(dir: &OsStr) -> Option<Vec<OsString>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            report(&[b"cannot open '", dir.as_bytes(), b"': ", &reason(&e)]);
            return None;
        }
    };
    let mut names = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => names.push(entry.file_name()),
            Err(e) => {
                report(&[b"error reading '", dir.as_bytes(), b"': ", &reason(&e)]);
                return None;
            }
        }
    }
    // OsString orders bytewise on Unix.
    names.sort_unstable();
    Some(names)
}

/// Handle one directory entry. Returns false if it should mark the run failed.
fn process<W: Write>(name: &OsStr, source: &OsStr, dest_base: &OsStr, out: &mut Out<W>) -> bool {
    let n = name.as_bytes();
    if n.starts_with(b".") {
        return true;
    }

    let src_path = join(source.as_bytes(), n);
    match fs::symlink_metadata(&src_path) {
        Ok(m) if m.file_type().is_file() => {}
        _ => return true,
    }

    let Some(ext) = extension(n) else {
        return true;
    };

    let dest_dir = join(dest_base.as_bytes(), ext);
    let dest_path = join(dest_dir.as_bytes(), n);

    if let Err(e) = fs::DirBuilder::new().recursive(true).mode(0o777).create(&dest_dir) {
        report(&[b"cannot create '", dest_dir.as_bytes(), b"': ", &reason(&e)]);
        return false;
    }

    let exists = |out: &mut Out<W>| {
        out.line(&[b"File ", n, b" already exists in ", dest_dir.as_bytes()]);
    };

    match fs::symlink_metadata(&dest_path) {
        Ok(_) => {
            exists(out);
            return true;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            report(&[b"cannot access '", dest_path.as_bytes(), b"': ", &reason(&e)]);
            return false;
        }
    }

    match move_file(Path::new(&src_path), Path::new(&dest_path)) {
        Ok(()) => {
            out.line(&[b"Moved ", n, b" to ", dest_dir.as_bytes()]);
            true
        }
        // Lost a race with something that created dest_path after the check.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            exists(out);
            true
        }
        Err(e) => {
            report(&[b"cannot move '", src_path.as_bytes(), b"' to '", dest_path.as_bytes(), b"': ", &reason(&e)]);
            false
        }
    }
}

/// Move without ever replacing an existing destination. Fails with
/// `AlreadyExists` if `dst` appeared.
fn move_file(src: &Path, dst: &Path) -> io::Result<()> {
    match rename_noclobber(src, dst) {
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => copy_then_delete(src, dst),
        r => r,
    }
}

/// Same-filesystem rename that fails with `AlreadyExists` rather than
/// replacing `dst`.
fn rename_noclobber(src: &Path, dst: &Path) -> io::Result<()> {
    match rename_noreplace(src, dst) {
        // The filesystem (or OS) can't do a no-clobber rename. Never fall back
        // to a plain rename: it would overwrite a dst created since the check.
        Err(e) if is_unsupported(&e) => link_then_unlink(src, dst),
        r => r,
    }
}

fn is_unsupported(e: &io::Error) -> bool {
    // ENOTSUP and EOPNOTSUPP are the same value on Linux but not on macOS.
    let unsupported = [libc::EINVAL, libc::ENOSYS, libc::ENOTSUP, libc::EOPNOTSUPP];
    e.raw_os_error().is_some_and(|n| unsupported.contains(&n))
}

fn cstring(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

#[cfg(target_os = "linux")]
fn rename_noreplace(src: &Path, dst: &Path) -> io::Result<()> {
    let (src, dst) = (cstring(src)?, cstring(dst)?);
    // Raw syscall rather than the libc wrapper so older glibc/musl work too.
    let r = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            src.as_ptr(),
            libc::AT_FDCWD,
            dst.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if r == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(target_os = "macos")]
fn rename_noreplace(src: &Path, dst: &Path) -> io::Result<()> {
    let (src, dst) = (cstring(src)?, cstring(dst)?);
    let r = unsafe { libc::renamex_np(src.as_ptr(), dst.as_ptr(), libc::RENAME_EXCL) };
    if r == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rename_noreplace(_src: &Path, _dst: &Path) -> io::Result<()> {
    Err(io::Error::from_raw_os_error(libc::ENOSYS))
}

/// Portable no-clobber move: `link` fails with `EEXIST` atomically. A
/// filesystem without hard links makes this fail too, which is reported as an
/// error rather than risking an overwrite.
fn link_then_unlink(src: &Path, dst: &Path) -> io::Result<()> {
    fs::hard_link(src, dst)?;
    if let Err(e) = fs::remove_file(src) {
        // dst is the same inode as src, so dropping it loses nothing.
        let _ = fs::remove_file(dst);
        return Err(e);
    }
    Ok(())
}

fn source_changed() -> io::Error {
    io::Error::other("source changed during move")
}

/// Cross-filesystem move. The copy is written to a hidden temporary file
/// beside `dst`, fsynced, renamed (no-clobber) into place, and the directory
/// fsynced before the source is deleted. So `dst` only ever appears complete
/// and durable: an interrupted run leaves at most a stray `.extmv-*` file,
/// never a truncated file under the real name. Any failure before the source
/// delete leaves the source intact and no `dst`; a failed source delete keeps
/// both copies.
fn copy_then_delete(src: &Path, dst: &Path) -> io::Result<()> {
    // The entry was a regular file when checked, but may have been swapped
    // since. Don't follow a symlink and don't block on a FIFO.
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(src)
        .map_err(|e| if e.raw_os_error() == Some(libc::ELOOP) { source_changed() } else { e })?;
    let meta = input.metadata()?;
    if !meta.is_file() {
        return Err(source_changed());
    }

    // dst always has a parent: process() builds it as join(dest_dir, name).
    let dir = dst.parent().unwrap_or(Path::new("."));
    let (tmp, output) = create_temp(dir)?;
    if let Err(e) = copy_contents(&mut input, output, &meta).and_then(|()| rename_noclobber(&tmp, dst)) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    // Make the new directory entry durable before the source goes away;
    // otherwise a crash could lose the file on both filesystems.
    if let Err(e) = sync_dir(dir) {
        let _ = fs::remove_file(dst);
        return Err(e);
    }

    // Only delete src if it still names the file that was copied; otherwise
    // keep both. (Narrows, but can't close, the window: there is no
    // "unlink if inode matches".)
    match fs::symlink_metadata(src) {
        Ok(now) if now.dev() == meta.dev() && now.ino() == meta.ino() => fs::remove_file(src),
        Ok(_) => Err(source_changed()),
        Err(e) => Err(e),
    }
}

/// Exclusively create a hidden temporary file in `dir`. The name doesn't
/// embed the file's own name, which could push it past `NAME_MAX`.
fn create_temp(dir: &Path) -> io::Result<(std::path::PathBuf, File)> {
    let pid = std::process::id();
    let mut n = 0u32;
    loop {
        let tmp = dir.join(format!(".extmv-{pid}-{n}.tmp"));
        match OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp) {
            Ok(file) => return Ok((tmp, file)),
            // A stray from an earlier, interrupted run with the same pid.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && n < 100 => n += 1,
            Err(e) => return Err(e),
        }
    }
}

/// fsync a directory. Filesystems that can't sync directories (some FUSE or
/// network ones) are tolerated: there is nothing more to be done there.
fn sync_dir(dir: &Path) -> io::Result<()> {
    match File::open(dir).and_then(|d| d.sync_all()) {
        Err(e) if is_unsupported(&e) => Ok(()),
        r => r,
    }
}

fn copy_contents(input: &mut File, mut output: File, meta: &Metadata) -> io::Result<()> {
    // Set the mode explicitly so umask doesn't alter it; setuid/setgid/sticky
    // are dropped on purpose.
    output.set_permissions(fs::Permissions::from_mode(meta.permissions().mode() & 0o777))?;
    io::copy(input, &mut output)?;
    // Timestamps are best-effort; failing to set them is not worth failing
    // (and undoing) the move.
    if let (Ok(atime), Ok(mtime)) = (meta.accessed(), meta.modified()) {
        let _ = output.set_times(FileTimes::new().set_accessed(atime).set_modified(mtime));
    }
    output.sync_all()?;
    close(output)
}

/// Close explicitly so the error is seen; dropping a `File` discards it.
fn close(file: File) -> io::Result<()> {
    let fd = file.into_raw_fd();
    if unsafe { libc::close(fd) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

fn run(source: &OsStr, dest_base: &OsStr) -> bool {
    let Some(names) = snapshot(source) else {
        return false;
    };

    let mut out = Out { w: BufWriter::new(io::stdout().lock()), failed: false };
    let mut ok = true;
    for name in &names {
        ok &= process(name, source, dest_base, &mut out);
    }

    if !out.finish() {
        report(&[b"error writing to stdout"]);
        ok = false;
    }
    ok
}

fn main() -> ExitCode {
    match parse_args(std::env::args_os().skip(1)) {
        Cli::Help => {
            let _ = io::stdout().write_all(USAGE.as_bytes());
            ExitCode::SUCCESS
        }
        Cli::Usage(msg) => {
            report(&[&msg]);
            let _ = io::stderr().write_all(USAGE.as_bytes());
            ExitCode::FAILURE
        }
        Cli::Run { source, dest_base } => {
            if run(&source, &dest_base) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    fn run_cli(source: &str, dest_base: &str) -> Cli {
        Cli::Run { source: source.into(), dest_base: dest_base.into() }
    }

    #[test]
    fn extension_rule() {
        assert_eq!(extension(b"report.pdf"), Some(&b"pdf"[..]));
        assert_eq!(extension(b"archive.tar.gz"), Some(&b"gz"[..]));
        assert_eq!(extension(b"A.JPG"), Some(&b"JPG"[..]));
        assert_eq!(extension(b"\xff.txt"), Some(&b"txt"[..]));
        assert_eq!(extension(b"noext"), None);
        assert_eq!(extension(b"foo."), None);
        assert_eq!(extension(b"a.."), None);
    }

    #[test]
    fn join_rule() {
        assert_eq!(join(b"Documents", b"pdf"), "Documents/pdf");
        assert_eq!(join(b"sorted/", b"log"), "sorted/log");
        assert_eq!(join(b"sorted//", b"log"), "sorted//log");
    }

    #[test]
    fn defaults() {
        assert_eq!(parse_args(args(&[])), run_cli(".", "Documents"));
        assert_eq!(parse_args(args(&["src"])), run_cli("src", "Documents"));
        assert_eq!(parse_args(args(&["src", "out"])), run_cli("src", "out"));
        assert_eq!(parse_args(args(&["-"])), run_cli("-", "Documents"));
    }

    #[test]
    fn first_decisive_arg_wins() {
        assert_eq!(parse_args(args(&["-h", "a", "b", "c"])), Cli::Help);
        assert_eq!(parse_args(args(&["a", "--help"])), Cli::Help);
        assert_eq!(parse_args(args(&["a", "b", "c", "-h"])), Cli::Usage(b"too many arguments".to_vec()));
        assert_eq!(parse_args(args(&["--bogus", "-h"])), Cli::Usage(b"unknown option '--bogus'".to_vec()));
    }

    #[test]
    fn empty_args_rejected() {
        assert_eq!(parse_args(args(&[".", ""])), Cli::Usage(b"empty path argument".to_vec()));
        assert_eq!(parse_args(args(&["", "x"])), Cli::Usage(b"empty path argument".to_vec()));
    }

    #[test]
    fn double_dash_ends_options() {
        assert_eq!(parse_args(args(&["--"])), run_cli(".", "Documents"));
        assert_eq!(parse_args(args(&["--", "-dir"])), run_cli("-dir", "Documents"));
        assert_eq!(parse_args(args(&["--", "-h"])), run_cli("-h", "Documents"));
        assert_eq!(parse_args(args(&["--", "a", "b", "c"])), Cli::Usage(b"too many arguments".to_vec()));
    }

    /// A fresh scratch directory, removed on drop.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("extmv-test-{}-{tag}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn link_then_unlink_moves() {
        let t = TempDir::new("link-ok");
        let (src, dst) = (t.0.join("a.txt"), t.0.join("b.txt"));
        fs::write(&src, "data").unwrap();
        link_then_unlink(&src, &dst).unwrap();
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(&dst).unwrap(), "data");
    }

    #[test]
    fn link_then_unlink_never_overwrites() {
        let t = TempDir::new("link-exists");
        let (src, dst) = (t.0.join("a.txt"), t.0.join("b.txt"));
        fs::write(&src, "new").unwrap();
        fs::write(&dst, "keep").unwrap();
        let e = link_then_unlink(&src, &dst).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&src).unwrap(), "new");
        assert_eq!(fs::read_to_string(&dst).unwrap(), "keep");
    }

    #[test]
    fn copy_sets_exact_mode_despite_umask() {
        let t = TempDir::new("copy-mode");
        let (src, dst) = (t.0.join("a.txt"), t.0.join("b.txt"));
        fs::write(&src, "data").unwrap();
        // 0o4666: setuid must be dropped; group/other write must survive umask.
        fs::set_permissions(&src, fs::Permissions::from_mode(0o4666)).unwrap();
        let old = unsafe { libc::umask(0o022) };
        let r = copy_then_delete(&src, &dst);
        unsafe { libc::umask(old) };
        r.unwrap();
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(&dst).unwrap(), "data");
        assert_eq!(fs::metadata(&dst).unwrap().permissions().mode() & 0o7777, 0o666);
    }

    fn entries(dir: &Path) -> Vec<OsString> {
        let mut names: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        names.sort();
        names
    }

    #[test]
    fn copy_leaves_no_temp_file() {
        let t = TempDir::new("copy-clean");
        fs::create_dir(t.0.join("out")).unwrap();
        let (src, dst) = (t.0.join("a.txt"), t.0.join("out/a.txt"));
        fs::write(&src, "data").unwrap();
        copy_then_delete(&src, &dst).unwrap();
        assert_eq!(fs::read_to_string(&dst).unwrap(), "data");
        assert_eq!(entries(&t.0.join("out")), ["a.txt"]);
    }

    #[test]
    fn copy_never_overwrites_and_cleans_up() {
        let t = TempDir::new("copy-exists");
        fs::create_dir(t.0.join("out")).unwrap();
        let (src, dst) = (t.0.join("a.txt"), t.0.join("out/a.txt"));
        fs::write(&src, "new").unwrap();
        fs::write(&dst, "keep").unwrap();
        let e = copy_then_delete(&src, &dst).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&src).unwrap(), "new");
        assert_eq!(fs::read_to_string(&dst).unwrap(), "keep");
        assert_eq!(entries(&t.0.join("out")), ["a.txt"]);
    }

    #[test]
    fn create_temp_skips_stale_names() {
        let t = TempDir::new("temp-stale");
        let stale = t.0.join(format!(".extmv-{}-0.tmp", std::process::id()));
        fs::write(&stale, "").unwrap();
        let (tmp, _) = create_temp(&t.0).unwrap();
        assert_ne!(tmp, stale);
        assert!(tmp.file_name().unwrap().as_bytes().starts_with(b".extmv-"));
    }

    #[test]
    fn copy_refuses_symlink_source() {
        let t = TempDir::new("copy-symlink");
        let (target, src, dst) = (t.0.join("target"), t.0.join("a.txt"), t.0.join("b.txt"));
        fs::write(&target, "data").unwrap();
        std::os::unix::fs::symlink(&target, &src).unwrap();
        let e = copy_then_delete(&src, &dst).unwrap_err();
        assert_eq!(e.to_string(), "source changed during move");
        assert!(src.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(!dst.exists());
    }

    #[test]
    fn copy_refuses_fifo_source_without_blocking() {
        let t = TempDir::new("copy-fifo");
        let (src, dst) = (t.0.join("a.txt"), t.0.join("b.txt"));
        let c = CString::new(src.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let e = copy_then_delete(&src, &dst).unwrap_err();
        assert_eq!(e.to_string(), "source changed during move");
        assert!(src.exists());
        assert!(!dst.exists());
    }

    #[test]
    fn reason_strips_os_error_suffix() {
        let e = io::Error::from_raw_os_error(libc::ENOENT);
        assert_eq!(reason(&e), b"No such file or directory");
    }
}
