//! extmv: move each regular file in a directory into `<dest_base>/<ext>/`.
//!
//! Rust port of the C `vacuum`/`extmv` tool; see the behavioral spec for the
//! exact contract. Differences from the C version, all sanctioned by the spec:
//!
//! - entries are processed in bytewise-sorted order;
//! - empty `source_dir`/`dest_base` arguments are rejected;
//! - `--` ends option parsing;
//! - the move is no-clobber (`renameat2(RENAME_NOREPLACE)` on Linux,
//!   `renamex_np(RENAME_EXCL)` on macOS), closing the check-then-rename race;
//! - the cross-filesystem fallback preserves atime/mtime and fsyncs the copy
//!   before deleting the source.

use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, FileTimes, Metadata, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::os::fd::IntoRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
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

    if let Err(e) = fs::DirBuilder::new()
        .recursive(true)
        .mode(0o777)
        .create(&dest_dir)
    {
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
            report(&[
                b"cannot access '",
                dest_path.as_bytes(),
                b"': ",
                &reason(&e),
            ]);
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
            report(&[
                b"cannot move '",
                src_path.as_bytes(),
                b"' to '",
                dest_path.as_bytes(),
                b"': ",
                &reason(&e),
            ]);
            false
        }
    }
}

/// Move without ever replacing an existing destination. Fails with
/// `AlreadyExists` if `dst` appeared.
fn move_file(src: &Path, dst: &Path) -> io::Result<()> {
    let renamed = match rename_noreplace(src, dst) {
        // The filesystem (or OS) can't do a no-clobber rename. Fall back to a
        // plain rename; the existence check just before this narrows, but
        // can't close, the window in which a new dst would be overwritten.
        Err(e) if is_unsupported(&e) => fs::rename(src, dst),
        r => r,
    };
    match renamed {
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => copy_then_delete(src, dst),
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
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn rename_noreplace(src: &Path, dst: &Path) -> io::Result<()> {
    let (src, dst) = (cstring(src)?, cstring(dst)?);
    let r = unsafe { libc::renamex_np(src.as_ptr(), dst.as_ptr(), libc::RENAME_EXCL) };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rename_noreplace(_src: &Path, _dst: &Path) -> io::Result<()> {
    Err(io::Error::from_raw_os_error(libc::ENOSYS))
}

/// Cross-filesystem move: exclusive-create the destination, copy, fsync,
/// close, then delete the source. A failed copy removes the partial
/// destination; a failed source delete keeps both copies.
fn copy_then_delete(src: &Path, dst: &Path) -> io::Result<()> {
    let mut input = File::open(src)?;
    let meta = input.metadata()?;
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(meta.permissions().mode() & 0o777)
        .open(dst)?;

    if let Err(e) = copy_contents(&mut input, output, &meta) {
        let _ = fs::remove_file(dst);
        return Err(e);
    }
    fs::remove_file(src)
}

fn copy_contents(input: &mut File, mut output: File, meta: &Metadata) -> io::Result<()> {
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
    if unsafe { libc::close(fd) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn run(source: &OsStr, dest_base: &OsStr) -> bool {
    let Some(names) = snapshot(source) else {
        return false;
    };

    let mut out = Out {
        w: BufWriter::new(io::stdout().lock()),
        failed: false,
    };
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
        Cli::Run {
            source: source.into(),
            dest_base: dest_base.into(),
        }
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
        assert_eq!(
            parse_args(args(&["a", "b", "c", "-h"])),
            Cli::Usage(b"too many arguments".to_vec())
        );
        assert_eq!(
            parse_args(args(&["--bogus", "-h"])),
            Cli::Usage(b"unknown option '--bogus'".to_vec())
        );
    }

    #[test]
    fn empty_args_rejected() {
        assert_eq!(
            parse_args(args(&[".", ""])),
            Cli::Usage(b"empty path argument".to_vec())
        );
        assert_eq!(
            parse_args(args(&["", "x"])),
            Cli::Usage(b"empty path argument".to_vec())
        );
    }

    #[test]
    fn double_dash_ends_options() {
        assert_eq!(parse_args(args(&["--"])), run_cli(".", "Documents"));
        assert_eq!(
            parse_args(args(&["--", "-dir"])),
            run_cli("-dir", "Documents")
        );
        assert_eq!(parse_args(args(&["--", "-h"])), run_cli("-h", "Documents"));
        assert_eq!(
            parse_args(args(&["--", "a", "b", "c"])),
            Cli::Usage(b"too many arguments".to_vec())
        );
    }

    #[test]
    fn reason_strips_os_error_suffix() {
        let e = io::Error::from_raw_os_error(libc::ENOENT);
        assert_eq!(reason(&e), b"No such file or directory");
    }
}
