//! Background service: watches the clipboard, owns the history, and performs
//! paste requests coming from the popup over a Unix socket.

use crate::{
    log_error, log_info, log_warn,
    paste::Paster,
    store::{self, Content, History},
};
use arboard::{Clipboard, Error as ClipError, ImageData};
use std::{
    borrow::Cow,
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    thread::{self, sleep},
    time::{Duration, Instant},
};

pub fn socket_path() -> PathBuf {
    store::runtime_dir().join("winv-clipboard.sock")
}

/// Larger images (raw RGBA) are skipped: they would cost hundreds of MB to keep and encode.
const MAX_IMAGE_BYTES: usize = 128 * 1024 * 1024;
/// Popup preview size limit, in pixels.
const THUMB_MAX: (u32, u32) = (480, 220);

/// A thread that panicked while holding a lock must not take the whole service down with it.
trait LockExt<T> {
    fn relock(&self) -> MutexGuard<'_, T>;
}
impl<T> LockExt<T> for Mutex<T> {
    fn relock(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

struct State {
    history: Mutex<History>,
    /// Used only to read what was copied. A huge copy can keep a read busy for many seconds, so
    /// pasting uses its own handle (`writer`) and never waits behind it.
    reader: Mutex<Clipboard>,
    writer: Mutex<Clipboard>,
    paster: Mutex<Paster>,
}

pub fn run() {
    // Exactly one service per user session, even if two start at the same moment.
    let _lock = match acquire_instance_lock() {
        Some(lock) => lock,
        None => {
            log_info!("daemon is already running; exiting");
            return;
        }
    };

    // The desktop may still be starting when we are launched at login; wait for it.
    let reader = open_clipboard_with_retry();
    let writer = open_clipboard_with_retry();

    let path = socket_path();
    let _ = fs::remove_file(&path); // stale socket from a previous run
    let listener = UnixListener::bind(&path).unwrap_or_else(|e| fatal(&format!("cannot bind {}: {e}", path.display())));
    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));

    crate::log::prune();
    crate::log::spawn_pruner();
    let history = History::load_at_startup().unwrap_or_else(|e| fatal(&format!("{e}; not starting, so that nothing is overwritten")));
    log_info!("daemon started: {} items in history, socket {}", history.items.len(), path.display());
    let state = Arc::new(State {
        history: Mutex::new(history),
        reader: Mutex::new(reader),
        writer: Mutex::new(writer),
        paster: Mutex::new(Paster::default()),
    });

    // Another program can rewrite GNOME's shortcut list and silently drop Super+V; put it back.
    thread::spawn(|| {
        let mut complained = false;
        loop {
            match crate::install::repair_shortcut() {
                Ok(true) => log_warn!("the Super+V shortcut had dropped out of GNOME's shortcut list; restored it"),
                Ok(false) => {}
                Err(e) if !complained => {
                    complained = true;
                    log_warn!("cannot check the Super+V shortcut: {e}");
                }
                Err(_) => {}
            }
            sleep(Duration::from_secs(10 * 60));
        }
    });

    let watcher_state = state.clone();
    thread::spawn(move || {
        // The watcher must never end: without it the service would keep answering the popup but
        // silently stop recording copies. (A panic is already in the log by the time we get here.)
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| watch_clipboard(watcher_state)));
        log_error!("the clipboard watcher stopped unexpectedly; restarting the service");
        sleep(Duration::from_secs(3)); // no tight loop if it keeps failing
        restart_self();
    });

    for stream in listener.incoming().flatten() {
        let state = state.clone();
        thread::spawn(move || handle_client(stream, state));
    }
}

fn fatal(msg: &str) -> ! {
    log_error!("{msg}");
    std::process::exit(1);
}

/// Non-blocking exclusive lock on a file in the runtime dir; released when the process exits.
fn acquire_instance_lock() -> Option<fs::File> {
    let _ = fs::create_dir_all(store::data_dir());
    let file = fs::OpenOptions::new().create(true).truncate(false).write(true).open(store::daemon_lock_path()).ok()?;
    // SAFETY: flock on a file descriptor we own.
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    locked.then_some(file)
}

fn open_clipboard_with_retry() -> Clipboard {
    let mut last_error = String::new();
    for attempt in 1..=30 {
        match Clipboard::new() {
            Ok(cb) => {
                if attempt > 1 {
                    log_info!("clipboard became available after {attempt} attempts");
                }
                return cb;
            }
            Err(e) => {
                last_error = e.to_string();
                if attempt == 1 {
                    log_warn!("clipboard not available yet ({e}); retrying for up to 30 s");
                }
                sleep(Duration::from_secs(1));
            }
        }
    }
    fatal(&format!("cannot open the clipboard: {last_error}"))
}

/// Replaces this process with a fresh copy of itself (same pid, clean connections).
fn restart_self() -> ! {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("winv-clipboard"));
    // After an upgrade the old binary shows up as "<path> (deleted)"; the new one is at <path>.
    let exe = exe.to_string_lossy().trim_end_matches(" (deleted)").to_string();
    let err = std::process::Command::new(&exe).arg("daemon").exec();
    fatal(&format!("cannot restart ({err})"));
}

// ---------------------------------------------------------------- watching

enum WatchError {
    /// XFixes can't be used at all: poll instead.
    Setup(String),
    /// The X connection died while running (XWayland restarted, ...): start over.
    Lost(String),
}

fn watch_clipboard(state: Arc<State>) {
    match wait_for_changes_xfixes(&state) {
        WatchError::Setup(e) => {
            log_warn!("XFixes unavailable ({e}); polling the clipboard instead");
            loop {
                check_clipboard(&state, None);
                sleep(Duration::from_millis(600));
            }
        }
        WatchError::Lost(e) => {
            log_error!("lost the connection to the display ({e}); restarting the service");
            sleep(Duration::from_secs(3));
            restart_self();
        }
    }
}

/// What the current clipboard owner offers, from its TARGETS list.
#[derive(Debug, Default)]
struct Offered {
    text: bool,
    image: bool,
    /// Marked by password managers (KeePassXC, KDE) as not to be recorded.
    secret: bool,
}

impl Offered {
    fn from_target_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Offered {
        let mut offered = Offered::default();
        for name in names {
            match name {
                "UTF8_STRING" | "STRING" | "TEXT" => offered.text = true,
                "x-kde-passwordManagerHint" => offered.secret = true,
                n if n.starts_with("text/plain") => offered.text = true,
                n if n.starts_with("image/") => offered.image = true,
                _ => {}
            }
        }
        offered
    }
}

/// Event driven: the X server (XWayland on GNOME/Wayland) tells us whenever the
/// CLIPBOARD selection gets a new owner. Only returns when something went wrong.
fn wait_for_changes_xfixes(state: &State) -> WatchError {
    use x11rb::{
        connection::Connection,
        protocol::{
            xfixes::{self, ConnectionExt as _, SelectionEventMask},
            xproto::{ConnectionExt as _, CreateWindowAux, WindowClass},
            Event,
        },
        COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT,
    };
    let setup = || -> Result<_, Box<dyn std::error::Error>> {
        let (conn, screen_num) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen_num].root;
        xfixes::query_version(&conn, 5, 0)?.reply()?;
        let win = conn.generate_id()?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            win,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            COPY_FROM_PARENT,
            &CreateWindowAux::new(),
        )?;
        let atom = |name: &[u8]| -> Result<u32, Box<dyn std::error::Error>> { Ok(conn.intern_atom(false, name)?.reply()?.atom) };
        let atoms = (atom(b"CLIPBOARD")?, atom(b"TARGETS")?, atom(b"WINV_TARGETS")?);
        conn.xfixes_select_selection_input(win, atoms.0, SelectionEventMask::SET_SELECTION_OWNER)?;
        conn.flush()?;
        Ok((conn, win, atoms))
    };
    let (conn, win, (clipboard, targets, property)) = match setup() {
        Ok(v) => v,
        Err(e) => return WatchError::Setup(e.to_string()),
    };
    log_info!("watching the clipboard via XFixes selection events");

    let mut changed = true; // capture whatever is on the clipboard at startup
    loop {
        // Wait for the next copy, unless one already arrived while we were busy.
        if !changed {
            loop {
                match conn.wait_for_event() {
                    Ok(Event::XfixesSelectionNotify(_)) => break,
                    Ok(_) => {}
                    Err(e) => return WatchError::Lost(e.to_string()),
                }
            }
        }
        sleep(Duration::from_millis(60)); // let the new owner finish setting up
        loop {
            match conn.poll_for_event() {
                Ok(Some(_)) => {} // coalesce bursts
                Ok(None) => break,
                Err(e) => return WatchError::Lost(e.to_string()),
            }
        }
        changed = false;

        // Ask the owner what it offers before reading anything: reading a format it doesn't
        // have costs a multi-second timeout per format.
        if let Err(e) = conn.convert_selection(win, clipboard, targets, property, x11rb::CURRENT_TIME).and_then(|_| conn.flush()) {
            return WatchError::Lost(e.to_string());
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut names: Option<Vec<String>> = None;
        while names.is_none() && Instant::now() < deadline {
            match conn.poll_for_event() {
                Ok(Some(Event::SelectionNotify(e))) if e.requestor == win => {
                    names = Some(if e.property == x11rb::NONE { Vec::new() } else { read_target_names(&conn, win, property) });
                }
                Ok(Some(Event::XfixesSelectionNotify(_))) => changed = true, // newer copy: look again after this one
                Ok(Some(_)) => {}
                Ok(None) => sleep(Duration::from_millis(5)),
                Err(e) => return WatchError::Lost(e.to_string()),
            }
        }
        match names {
            Some(names) => check_clipboard(state, Some(Offered::from_target_names(names.iter().map(String::as_str)))),
            None => log_warn!("the app that owns the clipboard did not answer within 1 s; skipped"),
        }
    }
}

fn read_target_names(conn: &impl x11rb::connection::Connection, win: u32, property: u32) -> Vec<String> {
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};
    let Some(reply) = conn.get_property(true, win, property, AtomEnum::ATOM, 0, 1024).ok().and_then(|c| c.reply().ok()) else {
        return Vec::new();
    };
    let atoms: Vec<u32> = reply.value32().map(Iterator::collect).unwrap_or_default();
    // Send all lookups first, then collect: one round trip instead of one per atom.
    let cookies: Vec<_> = atoms.iter().filter_map(|&a| conn.get_atom_name(a).ok()).collect();
    cookies.into_iter().filter_map(|c| c.reply().ok()).map(|r| String::from_utf8_lossy(&r.name).into_owned()).collect()
}

enum Clip {
    Text(String),
    Image(ImageData<'static>),
    None,
}

/// Reads what was copied. With `offered` (event mode) only formats the owner actually has are
/// requested; without it (polling mode) text is tried first, then an image.
fn read_clipboard(state: &State, offered: Option<&Offered>) -> Clip {
    let mut cb = state.reader.relock();
    let (try_text, try_image) = offered.map_or((true, true), |o| (o.text, o.image && !o.text));
    if try_text {
        match cb.get_text() {
            Ok(t) => return Clip::Text(t),
            Err(ClipError::ContentNotAvailable) if offered.is_none() => {}
            Err(_) => return Clip::None,
        }
    }
    if try_image && let Ok(img) = cb.get_image() {
        return Clip::Image(img.to_owned_img());
    }
    Clip::None
}

/// Records the current clipboard. Content we put there ourselves (a paste) is already the
/// newest item, so the duplicate check turns it into a no-op: no timing guesswork needed.
fn check_clipboard(state: &State, offered: Option<Offered>) {
    if offered.as_ref().is_some_and(|o| o.secret) {
        log_info!("clipboard changed: marked as a password by the app; not recorded");
        return;
    }
    let started = Instant::now();
    let clip = read_clipboard(state, offered.as_ref());
    let read_ms = started.elapsed().as_millis();
    if read_ms > 500 {
        log_warn!("reading the clipboard took {read_ms} ms (the app that owns it was slow to answer)");
    }

    let (what, changed) = match clip {
        Clip::Text(text) => {
            let what = format!("text, {} bytes", text.len());
            let changed = state.history.relock().add_text(text);
            (what, changed)
        }
        Clip::Image(img) => {
            let what = format!("image, {}x{}", img.width, img.height);
            let changed = add_image(state, &img);
            (what, changed)
        }
        Clip::None => ("no text or image".to_string(), false),
    };
    log_info!("clipboard changed: {what} -> {}", if changed { "added to history" } else { "ignored (duplicate/empty/too large)" });
    if changed && let Err(e) = state.history.relock().save() {
        log_error!("cannot save history: {e}");
    }
}

fn add_image(state: &State, img: &ImageData<'_>) -> bool {
    if img.bytes.len() > MAX_IMAGE_BYTES {
        log_warn!("image is too large to keep ({} MB raw)", img.bytes.len() / (1024 * 1024));
        return false;
    }
    let hash = store::hash_bytes(&img.bytes);
    // Encoding a big screenshot takes a while: do it without holding the history lock,
    // and skip it entirely for an image we already have.
    let known = state.history.relock().find_image(hash).is_some();
    let (png, thumb) = if known {
        (Vec::new(), Vec::new())
    } else {
        match (encode_png(img.width, img.height, &img.bytes), encode_thumbnail(img)) {
            (Some(png), Some(thumb)) => (png, thumb),
            _ => {
                log_error!("cannot encode the copied image");
                return false;
            }
        }
    };
    state.history.relock().add_image(&png, &thumb, img.width, img.height, hash).unwrap_or_else(|e| {
        log_error!("cannot store image: {e}");
        false
    })
}

fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Option<Vec<u8>> {
    use image::{codecs::png::PngEncoder, ExtendedColorType, ImageEncoder};
    let mut out = Vec::new();
    PngEncoder::new(&mut out).write_image(rgba, u32::try_from(width).ok()?, u32::try_from(height).ok()?, ExtendedColorType::Rgba8).ok()?;
    Some(out)
}

/// Popup-sized preview, so the popup never has to decode a full-size screenshot.
fn encode_thumbnail(img: &ImageData<'_>) -> Option<Vec<u8>> {
    let (w, h) = (u32::try_from(img.width).ok()?, u32::try_from(img.height).ok()?);
    let full = image::ImageBuffer::<image::Rgba<u8>, &[u8]>::from_raw(w, h, &img.bytes)?;
    let scale = (THUMB_MAX.0 as f32 / w as f32).min(THUMB_MAX.1 as f32 / h as f32).min(1.0);
    let (tw, th) = (((w as f32 * scale) as u32).max(1), ((h as f32 * scale) as u32).max(1));
    let small = image::imageops::thumbnail(&full, tw, th);
    encode_png(tw as usize, th as usize, small.as_raw())
}

// ---------------------------------------------------------------- commands

fn handle_client(stream: UnixStream, state: Arc<State>) {
    // A stuck or malicious local client must not hold a thread (or memory) forever.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    for line in BufReader::new(stream.take(64 * 1024)).lines().map_while(Result::ok) {
        let started = Instant::now();
        let result = run_command(&state, line.trim());
        match &result {
            Ok(()) => log_info!("command {:?} ok in {} ms", line.trim(), started.elapsed().as_millis()),
            Err(e) => log_error!("command {:?} failed in {} ms: {e}", line.trim(), started.elapsed().as_millis()),
        }
        let reply = match result {
            Ok(()) => "OK\n".to_string(),
            Err(e) => format!("ERR {e}\n"),
        };
        if writer.write_all(reply.as_bytes()).is_err() {
            break;
        }
    }
}

fn run_command(state: &Arc<State>, line: &str) -> Result<(), String> {
    let (cmd, arg) = line.split_once(' ').unwrap_or((line, ""));
    let id = || arg.parse::<u64>().map_err(|_| format!("{cmd} needs an item id"));
    match cmd {
        "PING" => Ok(()),
        "PASTE" => paste(state, id()?, false),
        "PASTE_TERMINAL" => paste(state, id()?, true),
        "DELETE" => {
            let id = id()?;
            mutate(state, |h| h.delete(id))
        }
        "PIN" => {
            let id = id()?;
            mutate(state, |h| h.toggle_pin(id))
        }
        "CLEAR" => mutate(state, History::clear),
        _ => Err(format!("unknown command {cmd:?}")),
    }
}

fn mutate(state: &State, f: impl FnOnce(&mut History)) -> Result<(), String> {
    let mut history = state.history.relock();
    f(&mut history);
    history.save().map_err(|e| e.to_string())
}

/// Puts the item on the clipboard, then (shortly after the popup has closed and focus has
/// returned to the previous window) presses Ctrl+V, or Ctrl+Shift+V for terminals.
fn paste(state: &Arc<State>, id: u64, terminal: bool) -> Result<(), String> {
    let content = {
        let mut history = state.history.relock();
        let content = history.get(id).ok_or("no such item")?.content.clone();
        history.bring_to_top(id);
        history.save().map_err(|e| e.to_string())?;
        content
    };

    // Decode before taking the clipboard lock so a big image can't hold up the clipboard watcher.
    let image = match &content {
        Content::Image { file, .. } => {
            let decoded = image::open(store::image_path(file)).map_err(|e| e.to_string())?.into_rgba8();
            Some(ImageData { width: decoded.width() as usize, height: decoded.height() as usize, bytes: Cow::Owned(decoded.into_raw()) })
        }
        Content::Text { .. } => None,
    };
    {
        let mut cb = state.writer.relock();
        match (content, image) {
            (Content::Text { text }, _) => cb.set_text(text).map_err(|e| e.to_string())?,
            (Content::Image { .. }, Some(image)) => cb.set_image(image).map_err(|e| e.to_string())?,
            (Content::Image { .. }, None) => return Err("image could not be read".into()),
        }
    }

    let state = state.clone();
    thread::spawn(move || {
        sleep(Duration::from_millis(250));
        state.paster.relock().paste(terminal);
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Offered;

    #[test]
    fn classifies_what_the_clipboard_owner_offers() {
        let text = Offered::from_target_names(["TARGETS", "UTF8_STRING", "text/plain;charset=utf-8"]);
        assert!(text.text && !text.image && !text.secret);

        let image = Offered::from_target_names(["TARGETS", "image/png"]);
        assert!(image.image && !image.text);

        let files = Offered::from_target_names(["TARGETS", "x-special/gnome-copied-files", "text/uri-list"]);
        assert!(!files.text && !files.image);

        let password = Offered::from_target_names(["UTF8_STRING", "x-kde-passwordManagerHint"]);
        assert!(password.secret);
    }
}
