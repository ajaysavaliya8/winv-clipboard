mod daemon;
mod install;
mod log;
mod paste;
mod store;
mod ui;
mod x11util;

use std::{
    os::unix::{net::UnixStream, process::CommandExt},
    process::Stdio,
    time::{Duration, Instant},
};

const USAGE: &str = "\
winv-clipboard - clipboard history for Ubuntu, opened with Super+V

USAGE:
    winv-clipboard install            install for this user: starts at every login, binds Super+V
    winv-clipboard uninstall          undo `install` (add --purge to delete history and logs too)
    winv-clipboard show               open / close the history popup (default)
    winv-clipboard daemon             run the background service (normally started by the installer)
    winv-clipboard log                print the last lines of the log (kept 1 day)
    winv-clipboard log errors         print the last errors (kept 5 days)";

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("daemon") => {
            log::init("daemon");
            daemon::run()
        }
        None | Some("show") => {
            log::init("popup");
            show()
        }
        Some("install") => {
            log::init("install");
            install::install()
        }
        Some("uninstall") => install::uninstall(args.next().as_deref() == Some("--purge")),
        Some("log") => print_log(args.next().as_deref() == Some("errors")),
        Some("-h" | "--help" | "help") => println!("{USAGE}"),
        Some(other) => {
            eprintln!("unknown command {other:?}\n\n{USAGE}");
            std::process::exit(2);
        }
    }
}

fn print_log(errors_only: bool) {
    let path = if errors_only { log::errors_path() } else { log::log_path() };
    println!("{}", path.display());
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    for line in &lines[lines.len().saturating_sub(40)..] {
        println!("{line}");
    }
}

/// True if `pid` is a running popup of this program (not just any process with that pid).
fn is_popup(pid: i32) -> bool {
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else { return false };
    let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).filter(|a| !a.is_empty()).collect();
    let program = args.first().map(|a| String::from_utf8_lossy(a).rsplit('/').next().unwrap_or("").to_string());
    program.as_deref() == Some("winv-clipboard") && (args.len() == 1 || args.get(1) == Some(&b"show".as_slice()))
}

/// Super+V while the popup is open closes it, so the shortcut acts as a toggle.
fn show() {
    let pidfile = store::popup_pidfile();
    let me = std::process::id() as i32;
    if let Some(pid) = std::fs::read_to_string(&pidfile).ok().and_then(|s| s.trim().parse::<i32>().ok())
        && pid != me
        && is_popup(pid)
    {
        log_info!("toggle: closing the open popup (pid {pid})");
        // SAFETY: plain signal delivery to a process we just identified as our own popup.
        unsafe { libc::kill(pid, libc::SIGTERM) };
        let _ = std::fs::remove_file(&pidfile);
        return;
    }
    let _ = std::fs::write(&pidfile, me.to_string());

    ensure_daemon_running();

    log_info!("popup: starting");
    let started = Instant::now();
    let result = ui::run();
    store::release_popup_pidfile();
    match result {
        Ok(()) => log_info!("popup: exited normally after {} ms", started.elapsed().as_millis()),
        Err(e) => {
            log_error!("popup failed: {e}");
            std::process::exit(1);
        }
    }
}

/// The service should always be up (autostart); start it on demand if it isn't.
fn ensure_daemon_running() {
    if UnixStream::connect(daemon::socket_path()).is_ok() {
        return;
    }
    log_warn!("clipboard service was not running; starting it");
    let Ok(exe) = std::env::current_exe() else { return };
    let spawned = std::process::Command::new(exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0) // outlive this popup
        .spawn();
    if let Err(e) = spawned {
        log_error!("could not start the clipboard service: {e}");
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && UnixStream::connect(daemon::socket_path()).is_err() {
        std::thread::sleep(Duration::from_millis(50));
    }
}
