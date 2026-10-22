//! Tiny file logger shared by the daemon and the popup.
//!
//! * `~/.local/share/winv-clipboard/winv.log`        everything, kept for 1 day
//! * `~/.local/share/winv-clipboard/winv-errors.log`  ERROR and PANIC lines only, kept for 5 days
//!
//! Clipboard *contents* are never logged, only sizes and ids. Old lines are dropped by [`prune`],
//! which only the daemon runs (at start-up and hourly), so two processes never rewrite a log at once.

use crate::store::data_dir;
use std::{
    fmt::Arguments,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::OnceLock,
};

const DAY_SECS: i64 = 24 * 60 * 60;
/// Retention of the full log, and of the errors-only log.
const LOG_KEEP_SECS: i64 = DAY_SECS;
const ERRORS_KEEP_SECS: i64 = 5 * DAY_SECS;
/// Hard cap per file, in case something logs in a tight loop; the newest lines win.
const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;

static ROLE: OnceLock<&'static str> = OnceLock::new();

pub fn log_path() -> PathBuf {
    data_dir().join("winv.log")
}

pub fn errors_path() -> PathBuf {
    data_dir().join("winv-errors.log")
}

/// Call once at startup. `role` shows up in every line ("daemon", "popup", ...).
pub fn init(role: &'static str) {
    let _ = ROLE.set(role);
    let _ = fs::DirBuilder::new().recursive(true).mode(0o700).create(data_dir());
    // Panics (including in worker threads) would otherwise vanish when started from a shortcut.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write("PANIC", format_args!("{info}"));
        default_hook(info);
    }));
}

/// "YYYY-MM-DD HH:MM:SS.mmm" in local time. The fixed width makes lines sort by time as text.
fn format_time(secs: i64, millis: u32) -> String {
    // SAFETY: localtime_r only writes into the tm we hand it.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t: libc::time_t = secs as libc::time_t;
        libc::localtime_r(&t, &mut tm);
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec,
            millis
        )
    }
}

fn now() -> (i64, u32) {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime only writes into the timespec we hand it.
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    #[allow(clippy::unnecessary_cast)] // tv_sec is 32-bit on some targets
    let secs = ts.tv_sec as i64;
    (secs, (ts.tv_nsec / 1_000_000) as u32)
}

fn append(path: &Path, line: &str) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).mode(0o600).open(path) {
        let _ = f.write_all(line.as_bytes());
    }
}

pub fn write(level: &str, msg: Arguments<'_>) {
    let role = ROLE.get().copied().unwrap_or("?");
    let (secs, millis) = now();
    let line = format!("{} {level:5} [{role}:{}] {msg}\n", format_time(secs, millis), std::process::id());
    if role != "install" {
        // The installer prints its own progress; the others log to stderr/journal too. Ignoring
        // errors on purpose: `eprint!` would panic (and so kill the service) on a closed pipe.
        let _ = std::io::stderr().write_all(line.as_bytes());
    }
    append(&log_path(), &line);
    if level == "ERROR" || level == "PANIC" {
        append(&errors_path(), &line);
    }
}

/// Drops lines older than the retention period from both log files.
pub fn prune() {
    let (secs, _) = now();
    prune_file(&log_path(), &format_time(secs - LOG_KEEP_SECS, 0));
    prune_file(&errors_path(), &format_time(secs - ERRORS_KEEP_SECS, 0));
    let _ = fs::remove_file(log_path().with_extension("log.1")); // left by an older version
}

/// Keeps lines whose timestamp is >= `cutoff`. Lines without a timestamp (the tail of a
/// multi-line panic message) stay with the line before them.
fn prune_file(path: &Path, cutoff: &str) {
    let Ok(text) = fs::read_to_string(path) else { return };
    let mut keeping = false;
    let mut kept: Vec<&str> = Vec::new();
    for line in text.lines() {
        let timestamp = line.get(..23).filter(|t| t.as_bytes()[0].is_ascii_digit() && t.as_bytes()[4] == b'-');
        if let Some(t) = timestamp {
            keeping = t >= cutoff;
        }
        if keeping {
            kept.push(line);
        }
    }
    // Enforce the size cap from the newest end.
    let mut size = 0;
    let first = kept
        .iter()
        .rposition(|l| {
            size += l.len() + 1;
            size > MAX_FILE_BYTES
        })
        .map_or(0, |i| i + 1);
    let kept = &kept[first..];

    if kept.len() == text.lines().count() {
        return; // nothing to drop
    }
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let written = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp);
    if written.and_then(|mut f| f.write_all(out.as_bytes())).is_ok() {
        let _ = fs::rename(&tmp, path);
    } else {
        let _ = fs::remove_file(&tmp);
    }
}

/// Keeps a long-running process (the daemon) trimmed.
pub fn spawn_pruner() {
    std::thread::spawn(|| loop {
        std::thread::sleep(std::time::Duration::from_secs(60 * 60));
        prune();
    });
}

#[macro_export]
macro_rules! log_info {
    ($($t:tt)*) => { $crate::log::write("INFO", format_args!($($t)*)) };
}
#[macro_export]
macro_rules! log_warn {
    ($($t:tt)*) => { $crate::log::write("WARN", format_args!($($t)*)) };
}
#[macro_export]
macro_rules! log_error {
    ($($t:tt)*) => { $crate::log::write("ERROR", format_args!($($t)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(age_secs: i64, level: &str, msg: &str) -> String {
        let (now, _) = now();
        format!("{} {level:5} [t:1] {msg}\n", format_time(now - age_secs, 0))
    }

    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("winv-log-test-{}-{name}", std::process::id()))
    }

    #[test]
    fn prune_drops_old_lines_but_keeps_panic_continuations() {
        let path = temp("a");
        let content = [
            line(3 * DAY_SECS, "INFO", "three days old"),
            line(2 * DAY_SECS, "PANIC", "old panic\n  stack frame of the old panic"),
            line(DAY_SECS / 2, "INFO", "twelve hours old"),
            line(60, "PANIC", "fresh panic\n  stack frame of the fresh panic"),
        ]
        .concat();
        fs::write(&path, content).unwrap();

        let (now, _) = now();
        prune_file(&path, &format_time(now - LOG_KEEP_SECS, 0));
        let left = fs::read_to_string(&path).unwrap();
        fs::remove_file(&path).unwrap();

        assert!(!left.contains("three days old") && !left.contains("old panic"), "{left}");
        assert!(!left.contains("stack frame of the old panic"), "{left}");
        assert!(left.contains("twelve hours old"), "{left}");
        assert!(left.contains("fresh panic") && left.contains("stack frame of the fresh panic"), "{left}");
    }

    #[test]
    fn errors_log_keeps_five_days() {
        let path = temp("b");
        fs::write(&path, [line(6 * DAY_SECS, "ERROR", "six days"), line(4 * DAY_SECS, "ERROR", "four days")].concat()).unwrap();
        let (now, _) = now();
        prune_file(&path, &format_time(now - ERRORS_KEEP_SECS, 0));
        let left = fs::read_to_string(&path).unwrap();
        fs::remove_file(&path).unwrap();
        assert!(!left.contains("six days") && left.contains("four days"), "{left}");
    }

    #[test]
    fn everything_old_empties_the_file() {
        let path = temp("c");
        fs::write(&path, line(10 * DAY_SECS, "INFO", "ancient")).unwrap();
        let (now, _) = now();
        prune_file(&path, &format_time(now - LOG_KEEP_SECS, 0));
        let left = fs::read_to_string(&path).unwrap();
        fs::remove_file(&path).unwrap();
        assert!(left.is_empty(), "{left}");
    }
}
