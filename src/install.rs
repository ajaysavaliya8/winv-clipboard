//! `install` / `uninstall`: per-user setup, no root and no extra permissions needed.
//!
//! * binary          ~/.local/bin/winv-clipboard
//! * start at login  ~/.config/autostart/winv-clipboard.desktop (runs `daemon` in every GNOME session)
//! * launcher        ~/.local/share/applications/winv-clipboard.desktop
//! * Super+V         a GNOME custom shortcut running `winv-clipboard show`

use crate::{log_error, log_info, store};
use std::{
    fs,
    os::unix::{net::UnixStream, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const BIN_NAME: &str = "winv-clipboard";
const KEY_PATH: &str = "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/winv-clipboard/";
const MEDIA_KEYS: &str = "org.gnome.settings-daemon.plugins.media-keys";
const KEY_SCHEMA: &str = "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding";
const SHELL_KEYS: &str = "org.gnome.shell.keybindings";
const SHORTCUT: &str = "<Super>v";

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| fail("HOME is not set")))
}

fn fail<T>(msg: &str) -> T {
    log_error!("{msg}");
    eprintln!("error: {msg}");
    std::process::exit(1);
}

fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config"))
}

fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".local/share"))
}

fn gsettings(args: &[&str]) -> Result<String, String> {
    let out = Command::new("gsettings").args(args).output().map_err(|e| format!("gsettings is not available: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Parses gsettings' `['a', 'b']` / `@as []` string-array output.
fn parse_list(s: &str) -> Vec<String> {
    s.split('\'').skip(1).step_by(2).map(str::to_string).collect()
}

fn format_list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|i| format!("'{i}'")).collect();
    format!("[{}]", quoted.join(", "))
}

/// Quotes a program path for a shell-style command line, which is how GNOME runs a custom
/// shortcut (paths with spaces, quotes, ... are legal).
fn shell_quote(path: &str) -> String {
    if path.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c)) {
        return path.to_string();
    }
    let escaped: String = path.chars().flat_map(|c| if "\"`$\\".contains(c) { vec!['\\', c] } else { vec![c] }).collect();
    format!("\"{escaped}\"")
}

/// The same path for a `.desktop` Exec line. The file format applies its own backslash escaping
/// before the quoting is read (so every backslash is doubled), and `%` would start a field code.
fn desktop_exec_quote(path: &str) -> String {
    shell_quote(path).replace('\\', "\\\\").replace('%', "%%")
}

fn desktop_entry(name: &str, exec: &str, extra: &str) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName={name}\nComment=Clipboard history (Super+V)\nExec={exec}\n\
         Icon=edit-paste\nTerminal=false\nStartupNotify=false\n{extra}"
    )
}

// ------------------------------------------------------------------ processes

/// Pids of running `winv-clipboard daemon` processes (excluding this one).
fn daemon_pids() -> Vec<i32> {
    let me = std::process::id() as i32;
    let Ok(entries) = fs::read_dir("/proc") else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().parse::<i32>().ok())
        .filter(|&pid| pid != me)
        .filter(|&pid| {
            let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) else { return false };
            let mut args = cmdline.split(|b| *b == 0);
            let program = args.next().map(|a| String::from_utf8_lossy(a).rsplit('/').next().unwrap_or("").to_string());
            program.as_deref() == Some(BIN_NAME) && args.next() == Some(b"daemon".as_slice())
        })
        .collect()
}

fn stop_daemons() {
    let pids = daemon_pids();
    for &pid in &pids {
        // SAFETY: plain signal delivery to processes we just identified as our own service.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && !daemon_pids().is_empty() {
        std::thread::sleep(Duration::from_millis(50));
    }
    for pid in daemon_pids() {
        // SAFETY: as above; the service ignored SIGTERM.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

fn start_daemon(binary: &Path) -> std::io::Result<()> {
    Command::new(binary)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0) // not tied to this terminal
        .spawn()
        .map(|_| ())
}

fn service_reachable() -> bool {
    UnixStream::connect(crate::daemon::socket_path()).is_ok()
}

// ------------------------------------------------------------------ install

pub fn install() {
    let dest = home().join(".local/bin").join(BIN_NAME);
    let exe = std::env::current_exe().unwrap_or_else(|e| fail(&format!("cannot locate this program: {e}")));
    fs::create_dir_all(dest.parent().unwrap()).unwrap_or_else(|e| fail(&format!("cannot create ~/.local/bin: {e}")));
    if exe != dest {
        // Replace via rename so a running copy is never overwritten in place.
        let tmp = dest.with_extension("new");
        fs::copy(&exe, &tmp).unwrap_or_else(|e| fail(&format!("cannot copy the program: {e}")));
        fs::rename(&tmp, &dest).unwrap_or_else(|e| fail(&format!("cannot install the program: {e}")));
    }
    println!("[1/4] program installed: {}", dest.display());
    let command = shell_quote(&dest.to_string_lossy());
    let exec = desktop_exec_quote(&dest.to_string_lossy());

    let autostart = config_home().join("autostart");
    fs::create_dir_all(&autostart).unwrap_or_else(|e| fail(&format!("cannot create {}: {e}", autostart.display())));
    fs::write(
        autostart.join("winv-clipboard.desktop"),
        desktop_entry(
            "Clipboard history service",
            &format!("{exec} daemon"),
            "NoDisplay=true\nX-GNOME-Autostart-enabled=true\nX-GNOME-Autostart-Delay=2\n",
        ),
    )
    .unwrap_or_else(|e| fail(&format!("cannot write the autostart entry: {e}")));
    let apps = data_home().join("applications");
    fs::create_dir_all(&apps).unwrap_or_else(|e| fail(&format!("cannot create {}: {e}", apps.display())));
    fs::write(
        apps.join("winv-clipboard.desktop"),
        desktop_entry("Clipboard history", &format!("{exec} show"), "Categories=Utility;\nKeywords=clipboard;paste;history;copy;\n"),
    )
    .unwrap_or_else(|e| fail(&format!("cannot write the launcher entry: {e}")));
    println!("[2/4] starts automatically at every login (and appears in the app launcher)");

    match bind_shortcut(&format!("{command} show")) {
        Ok(()) => println!("[3/4] Super+V opens the clipboard history"),
        Err(e) => {
            log_error!("could not bind Super+V: {e}");
            eprintln!("[3/4] could not bind Super+V ({e}).\n      Bind `{} show` to a key in Settings > Keyboard > Shortcuts.", dest.display());
        }
    }

    stop_daemons(); // so the new program is the one running
    match start_daemon(&dest) {
        Ok(()) => {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline && !service_reachable() {
                std::thread::sleep(Duration::from_millis(100));
            }
            if service_reachable() {
                println!("[4/4] clipboard service is running");
                println!("\nDone. Copy something, then press Super+V. It starts by itself at every login.");
                let bin_dir = dest.parent().unwrap();
                if !std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d == bin_dir)) {
                    println!("Note: {} is not in this shell's PATH yet; Ubuntu adds it at your next login.", bin_dir.display());
                }
            } else {
                eprintln!("[4/4] the service did not come up; see `{BIN_NAME} log errors`");
            }
        }
        Err(e) => {
            log_error!("could not start the service: {e}");
            eprintln!("[4/4] could not start the service: {e}");
        }
    }
    log_info!("install finished");
}

fn bind_shortcut(command: &str) -> Result<(), String> {
    // GNOME uses Super+V for the notification tray; free it first.
    let tray = parse_list(&gsettings(&["get", SHELL_KEYS, "toggle-message-tray"])?);
    let tray: Vec<String> = tray.into_iter().filter(|k| !k.eq_ignore_ascii_case(SHORTCUT)).collect();
    gsettings(&["set", SHELL_KEYS, "toggle-message-tray", &format_list(&tray)])?;

    let mut paths = parse_list(&gsettings(&["get", MEDIA_KEYS, "custom-keybindings"])?);
    if !paths.iter().any(|p| p == KEY_PATH) {
        paths.push(KEY_PATH.to_string());
        gsettings(&["set", MEDIA_KEYS, "custom-keybindings", &format_list(&paths)])?;
    }
    let schema = format!("{KEY_SCHEMA}:{KEY_PATH}");
    gsettings(&["set", &schema, "name", "Clipboard history"])?;
    gsettings(&["set", &schema, "command", command])?;
    gsettings(&["set", &schema, "binding", SHORTCUT])?;

    // Read it back: a shortcut that silently didn't stick is the worst kind of failure.
    let bound = gsettings(&["get", &schema, "binding"])?;
    if !bound.contains(SHORTCUT) {
        return Err(format!("GNOME reports the shortcut as {bound}"));
    }
    Ok(())
}

/// True when GNOME's list of custom shortcuts has lost our entry while the entry itself is still
/// set up: something else rewrote the list (adding another shortcut can do that). An entry that
/// was removed on purpose (no binding left, as after `uninstall`) is not touched, and neither is
/// one the user re-bound to another key.
fn shortcut_lost(list: &[String], binding: &str) -> bool {
    !list.iter().any(|p| p == KEY_PATH) && !binding.trim().trim_matches('\'').is_empty()
}

/// Puts our entry back into GNOME's list if it fell out of it. Called by the service, so Super+V
/// repairs itself. Returns whether anything was repaired.
pub fn repair_shortcut() -> Result<bool, String> {
    let mut list = parse_list(&gsettings(&["get", MEDIA_KEYS, "custom-keybindings"])?);
    let binding = gsettings(&["get", &format!("{KEY_SCHEMA}:{KEY_PATH}"), "binding"])?;
    if !shortcut_lost(&list, &binding) {
        return Ok(false);
    }
    list.push(KEY_PATH.to_string());
    gsettings(&["set", MEDIA_KEYS, "custom-keybindings", &format_list(&list)])?;
    Ok(true)
}

// ------------------------------------------------------------------ uninstall

pub fn uninstall(purge: bool) {
    stop_daemons();
    let _ = fs::remove_file(config_home().join("autostart/winv-clipboard.desktop"));
    let _ = fs::remove_file(data_home().join("applications/winv-clipboard.desktop"));
    let _ = fs::remove_file(home().join(".local/bin").join(BIN_NAME));

    if let Ok(list) = gsettings(&["get", MEDIA_KEYS, "custom-keybindings"]) {
        let paths: Vec<String> = parse_list(&list).into_iter().filter(|p| p != KEY_PATH).collect();
        let _ = gsettings(&["set", MEDIA_KEYS, "custom-keybindings", &format_list(&paths)]);
    }
    let schema = format!("{KEY_SCHEMA}:{KEY_PATH}");
    for key in ["name", "command", "binding"] {
        let _ = gsettings(&["reset", &schema, key]);
    }
    let _ = gsettings(&["reset", SHELL_KEYS, "toggle-message-tray"]); // Super+V goes back to the tray

    if purge {
        let _ = fs::remove_dir_all(store::data_dir());
        let _ = fs::remove_dir_all(store::config_dir());
        println!("uninstalled; history, logs and settings deleted");
    } else {
        println!("uninstalled (your history and logs in {} were kept; add --purge to delete them)", store::data_dir().display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gsettings_lists() {
        assert_eq!(parse_list("['<Super>v', '<Super>m']"), ["<Super>v", "<Super>m"]);
        assert!(parse_list("@as []").is_empty());
        assert!(parse_list("[]").is_empty());
        assert_eq!(parse_list("['/a/b/']"), ["/a/b/"]);
    }

    #[test]
    fn formats_gsettings_lists() {
        assert_eq!(format_list(&[]), "[]");
        assert_eq!(format_list(&["a".into(), "b".into()]), "['a', 'b']");
        assert_eq!(parse_list(&format_list(&["x".into(), "y".into()])), ["x", "y"]);
    }

    #[test]
    fn a_shortcut_that_fell_out_of_the_list_is_detected() {
        let other = vec!["/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/custom0/".to_string()];
        let ours = vec![KEY_PATH.to_string()];
        assert!(shortcut_lost(&other, "'<Super>v'"), "list rewritten, entry still set up");
        assert!(shortcut_lost(&[], "'<Super>v'"));
        assert!(!shortcut_lost(&ours, "'<Super>v'"), "healthy");
        assert!(!shortcut_lost(&other, "''"), "removed on purpose (uninstall): leave it");
        assert!(shortcut_lost(&other, "'<Super><Shift>v'"), "re-bound by the user, but still dropped from the list");
    }

    #[test]
    fn shortcut_paths_are_quoted_only_when_needed() {
        assert_eq!(shell_quote("/home/me/.local/bin/winv-clipboard"), "/home/me/.local/bin/winv-clipboard");
        assert_eq!(shell_quote("/home/my name/bin/winv"), "\"/home/my name/bin/winv\"");
        assert_eq!(shell_quote("/h/$x/b"), "\"/h/\\$x/b\"");
    }

    #[test]
    fn desktop_exec_paths_get_the_file_formats_extra_escaping() {
        assert_eq!(desktop_exec_quote("/home/me/.local/bin/winv-clipboard"), "/home/me/.local/bin/winv-clipboard");
        assert_eq!(desktop_exec_quote("/home/my name/bin/winv"), "\"/home/my name/bin/winv\"");
        // GLib refuses to load an entry with a single backslash here (checked with `gio launch`).
        assert_eq!(desktop_exec_quote("/h/$x/b"), "\"/h/\\\\$x/b\"");
        assert_eq!(desktop_exec_quote("/a\\b"), "\"/a\\\\\\\\b\"");
        assert_eq!(desktop_exec_quote("/100%/b"), "\"/100%%/b\"");
    }
}
