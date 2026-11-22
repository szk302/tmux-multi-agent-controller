//! Small helpers: names (§3.1), time stamps (§4.2), atomic files (§4.3),
//! text hashing (§7.4) and signals.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use sha2::{Digest, Sha256};
use time::format_description::BorrowedFormatItem;
use time::macros::format_description;
use time::{OffsetDateTime, PrimitiveDateTime};

use crate::error::{Error, Result};

/// Whether `s` is a valid agent name or hold key: `a-z 0-9 . _ -`, starting
/// with `a-z 0-9`, 1 to 64 characters (§3.1).
pub fn valid_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|&c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'.' || c == b'_' || c == b'-'
        })
}

/// Validate an agent name, as a usage error.
pub fn check_name(name: &str) -> Result<()> {
    if valid_name(name) {
        Ok(())
    } else {
        Err(Error::usage(format!(
            "invalid agent name {name:?}: use 1-64 of a-z 0-9 . _ - starting with a-z or 0-9"
        )))
    }
}

/// Whether `s` is a valid UUID v4 string in lower case with hyphens.
pub fn valid_instance(s: &str) -> bool {
    uuid::Uuid::parse_str(s).is_ok_and(|u| u.hyphenated().to_string() == s)
}

const STAMP: &[BorrowedFormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]Z");

/// Current time as UTC RFC 3339 with microseconds (§4.2).
pub fn now_stamp() -> String {
    stamp(OffsetDateTime::now_utc())
}

pub fn stamp(t: OffsetDateTime) -> String {
    t.format(STAMP).unwrap_or_else(|_| t.to_string())
}

/// Parse a time stamp written by [`now_stamp`].
pub fn parse_stamp(s: &str) -> Option<OffsetDateTime> {
    let body = s.strip_suffix('Z')?;
    let fmt =
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]");
    PrimitiveDateTime::parse(body, fmt)
        .ok()
        .map(|t| t.assume_utc())
}

/// Seconds elapsed since a time stamp (0 if it cannot be parsed or is in the future).
/// The largest number of seconds tmac takes (about 31 years), so that a
/// time limit always makes a valid `Duration` and deadline.
pub const MAX_SECONDS: f64 = 1e9;

/// Whether `n` is a number of seconds tmac takes: from 0 to
/// [`MAX_SECONDS`] (not NaN or infinite).
pub fn valid_seconds(n: f64) -> bool {
    (0.0..=MAX_SECONDS).contains(&n)
}

/// Parse a number of seconds from the command line (`--timeout`,
/// `--human-hold`).
pub fn parse_seconds(s: &str) -> std::result::Result<f64, String> {
    let n: f64 = s
        .trim()
        .parse()
        .map_err(|_| format!("{s:?} is not a number"))?;
    if valid_seconds(n) {
        Ok(n)
    } else {
        Err(format!("must be from 0 to {MAX_SECONDS} seconds"))
    }
}

pub fn elapsed_secs(s: &str) -> f64 {
    parse_stamp(s)
        .map(|t| (OffsetDateTime::now_utc() - t).as_seconds_f64().max(0.0))
        .unwrap_or(0.0)
}

/// Write `data` to `path` through a temporary file in the same directory and
/// `rename`, so readers never see a partial file (§4.3).
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::runtime(format!("no parent directory: {}", path.display())))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let r = (|| -> io::Result<()> {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if let Err(e) = r {
        let _ = fs::remove_file(&tmp);
        return Err(Error::runtime(format!(
            "cannot write {}: {e}",
            path.display()
        )));
    }
    Ok(())
}

/// Read a file, `None` if it does not exist.
pub fn read_opt(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::runtime(format!(
            "cannot read {}: {e}",
            path.display()
        ))),
    }
}

/// Remove a file, ignoring "not found".
pub fn remove_opt(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::runtime(format!(
            "cannot remove {}: {e}",
            path.display()
        ))),
    }
}

/// Append one line to a file.
pub fn append_line(path: &Path, line: &str) -> Result<()> {
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(())
}

fn cstr(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

/// Rename `from` to `to`, failing with `AlreadyExists` if `to` exists (§4.3).
#[cfg(target_os = "linux")]
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let f = cstr(from)?;
    let t = cstr(to)?;
    // SAFETY: valid NUL-terminated strings; renameat2 does not retain pointers.
    let r = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            f.as_ptr(),
            libc::AT_FDCWD,
            t.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Rename `from` to `to`, failing with `AlreadyExists` if `to` exists (§4.3).
#[cfg(target_os = "macos")]
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let f = cstr(from)?;
    let t = cstr(to)?;
    // SAFETY: valid NUL-terminated strings.
    let r = unsafe { libc::renamex_np(f.as_ptr(), t.as_ptr(), libc::RENAME_EXCL) };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Whether an error means the destination exists.
pub fn is_exists(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::AlreadyExists
        || matches!(e.raw_os_error(), Some(libc::EEXIST) | Some(libc::ENOTEMPTY))
}

/// Normalize text for comparison: line endings to LF, surrounding white
/// space removed (§7.4).
pub fn normalize_text(s: &str) -> String {
    s.replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim()
        .to_string()
}

/// SHA-256 of the normalized text, as `sha256:<hex>`.
pub fn text_hash(s: &str) -> String {
    let digest = Sha256::digest(normalize_text(s).as_bytes());
    let mut out = String::from("sha256:");
    for b in digest.iter() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Flag set to the number of the first SIGINT or SIGTERM received.
pub fn signal_flag() -> Arc<AtomicUsize> {
    let flag = Arc::new(AtomicUsize::new(0));
    for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        let _ = signal_hook::flag::register_usize(sig, Arc::clone(&flag), sig as usize);
    }
    flag
}

/// Return an error if a signal was received.
pub fn check_signal(flag: &AtomicUsize) -> Result<()> {
    match flag.load(Ordering::SeqCst) {
        0 => Ok(()),
        s => Err(Error::Signal(s as i32)),
    }
}

/// Sleep in short steps, returning early with an error on a signal.
pub fn sleep_checked(d: Duration, flag: &AtomicUsize) -> Result<()> {
    let step = Duration::from_millis(50);
    let mut left = d;
    while !left.is_zero() {
        check_signal(flag)?;
        let s = left.min(step);
        thread::sleep(s);
        left -= s;
    }
    check_signal(flag)
}

/// Quote a string for `/bin/sh` if needed.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"/._-+=:@%,".contains(&c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Start time of a process, used to recognize it later (§11). `None` if
/// the process does not exist.
#[cfg(target_os = "linux")]
pub fn process_start_time(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name (field 2) is in parentheses and may contain spaces.
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    // A zombie has ended; it only waits to be reaped (no init in some containers).
    if fields.next()? == "Z" {
        return None;
    }
    fields.nth(18).map(str::to_string)
}

#[cfg(not(target_os = "linux"))]
pub fn process_start_time(pid: u32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_ps_start(&String::from_utf8_lossy(&out.stdout))
}

/// Start time from `ps -o stat= -o lstart=`; `None` for a zombie, which has
/// ended and only waits to be reaped.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn parse_ps_start(out: &str) -> Option<String> {
    let (stat, start) = out.trim().split_once(char::is_whitespace)?;
    let start = start.trim();
    (!stat.starts_with('Z') && !start.is_empty()).then(|| start.to_string())
}

/// Whether `pid` is still the process that started at `start`.
pub fn same_process(pid: u32, start: &str) -> bool {
    process_start_time(pid).as_deref() == Some(start)
}

/// Open (creating) a file used only as a lock.
pub fn open_lock_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| Error::runtime(format!("cannot open lock file {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for ok in ["a", "impl-001", "a.b_c", "0x", &"a".repeat(64)] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "A", "-a", ".a", "a/b", "a b", &"a".repeat(65), "日本"] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn stamps_round_trip() {
        let s = now_stamp();
        assert!(s.ends_with('Z') && s.len() == 27, "{s}");
        assert!(parse_stamp(&s).is_some());
        assert!(elapsed_secs(&s) < 5.0);
    }

    #[test]
    fn hashing_normalizes() {
        assert_eq!(text_hash("a\r\nb\n"), text_hash("  a\nb"));
        assert_ne!(text_hash("a"), text_hash("b"));
        assert!(text_hash("x").starts_with("sha256:"));
    }

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("/usr/bin/tmac"), "/usr/bin/tmac");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn ps_output() {
        assert_eq!(
            parse_ps_start("Ss   Sat Nov 21 04:29:43 2026\n").as_deref(),
            Some("Sat Nov 21 04:29:43 2026")
        );
        assert_eq!(parse_ps_start("Z    Sat Nov 21 04:29:43 2026\n"), None);
        assert_eq!(parse_ps_start(""), None);
    }

    #[test]
    fn zombies_count_as_ended() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        // Not reaped yet: a zombie.
        for _ in 0..100 {
            if process_start_time(pid).is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(process_start_time(pid).is_none());
        child.wait().unwrap();
    }

    #[test]
    fn process_identity() {
        let me = std::process::id();
        let start = process_start_time(me).unwrap();
        assert!(same_process(me, &start));
        assert!(!same_process(me, "0"));
    }
}
