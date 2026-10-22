//! The Win+V style popup.

use crate::{
    daemon::socket_path,
    log_error, log_info, log_warn,
    store::{self, Content, History},
    x11util::{self, CLICKED_OUTSIDE},
};
use eframe::egui::{
    self, text::LayoutJob, Align, Align2, Color32, CornerRadius, FontId, Id, Key, Margin, Modifiers, Pos2, Rect,
    RichText, Sense, Stroke, StrokeKind, TextureHandle, Vec2,
};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

const WIN_SIZE: Vec2 = Vec2::new(372.0, 520.0);
const TEXT_ROWS: usize = 4;
/// How long the popup waits for keyboard focus before giving up and closing itself.
const FOCUS_GRACE: Duration = Duration::from_secs(3);

// ------------------------------------------------------------------ theme

#[derive(Clone, Copy)]
struct Theme {
    dark: bool,
    bg: Color32,
    border: Color32,
    card: Color32,
    card_hover: Color32,
    text: Color32,
    dim: Color32,
    accent: Color32,
}

impl Theme {
    /// Follows GNOME's light/dark setting (dark if it can't be read).
    fn detect() -> Theme {
        let dark = std::process::Command::new("gsettings")
            .args(["get", "org.gnome.desktop.interface", "color-scheme"])
            .output()
            .map(|o| !o.status.success() || String::from_utf8_lossy(&o.stdout).contains("dark"))
            .unwrap_or(true);
        if dark {
            Theme {
                dark,
                bg: Color32::from_rgb(0x20, 0x20, 0x20),
                border: Color32::from_rgb(0x45, 0x45, 0x45),
                card: Color32::from_rgb(0x2d, 0x2d, 0x2d),
                card_hover: Color32::from_rgb(0x3a, 0x3a, 0x3a),
                text: Color32::WHITE,
                dim: Color32::from_rgb(0xa0, 0xa0, 0xa0),
                accent: Color32::from_rgb(0x60, 0xcd, 0xff),
            }
        } else {
            Theme {
                dark,
                bg: Color32::from_rgb(0xf3, 0xf3, 0xf3),
                border: Color32::from_rgb(0xcc, 0xcc, 0xcc),
                card: Color32::from_rgb(0xfb, 0xfb, 0xfb),
                card_hover: Color32::from_rgb(0xe9, 0xe9, 0xe9),
                text: Color32::from_rgb(0x1a, 0x1a, 0x1a),
                dim: Color32::from_rgb(0x60, 0x60, 0x60),
                accent: Color32::from_rgb(0x00, 0x5f, 0xb8),
            }
        }
    }

    fn style(&self, ctx: &egui::Context) {
        let mut style = (*ctx.global_style()).clone();
        style.visuals = if self.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
        style.visuals.override_text_color = Some(self.text);
        style.spacing.item_spacing = Vec2::new(8.0, 8.0);
        style.spacing.scroll.bar_width = 6.0;
        ctx.set_global_style(style);
    }
}

/// egui's built-in font has no Gujarati/Devanagari/CJK etc.; borrow any that the system has.
/// Done before the window exists so a slow disk can't make a visible window look frozen.
fn load_fonts() -> egui::FontDefinitions {
    const CANDIDATES: &[&str] = &[
        "Ubuntu-R.ttf",
        "NotoSans-Regular.ttf",
        "NotoSansGujarati-Regular.ttf",
        "NotoSansDevanagari-Regular.ttf",
        "NotoSansBengali-Regular.ttf",
        "NotoSansTamil-Regular.ttf",
        "NotoSansTelugu-Regular.ttf",
        "NotoSansGurmukhi-Regular.ttf",
        "NotoSansArabic-Regular.ttf",
        "NotoSansCJK-Regular.ttc",
        "DejaVuSans.ttf",
    ];
    let mut found: Vec<(String, Vec<u8>)> = Vec::new();
    let mut stack = vec![std::path::PathBuf::from("/usr/share/fonts")];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            if CANDIDATES.contains(&name)
                && !found.iter().any(|(n, _)| n == name)
                && let Ok(bytes) = std::fs::read(&path)
            {
                found.push((name.to_string(), bytes));
            }
        }
    }
    let mut fonts = egui::FontDefinitions::default();
    for (name, bytes) in found {
        fonts.font_data.insert(name.clone(), Arc::new(egui::FontData::from_owned(bytes)));
        // Ubuntu goes first so the popup matches the desktop; the rest are glyph fallbacks.
        let list = fonts.families.entry(egui::FontFamily::Proportional).or_default();
        if name.starts_with("Ubuntu") {
            list.insert(0, name);
        } else {
            list.push(name);
        }
    }
    fonts
}

// ------------------------------------------------------------------ daemon link

/// Sends one command to the daemon and waits for its reply.
fn send(cmd: &str) -> Result<(), String> {
    let mut stream = UnixStream::connect(socket_path()).map_err(|_| "The clipboard service is not running.".to_string())?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(3))).ok();
    writeln!(stream, "{cmd}").map_err(|e| e.to_string())?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).map_err(|e| e.to_string())?;
    match reply.trim() {
        "OK" => Ok(()),
        other => Err(other.trim_start_matches("ERR ").to_string()),
    }
}

// ------------------------------------------------------------------ diagnostics

static START: OnceLock<Instant> = OnceLock::new();
/// Milliseconds since START (+1) at which the UI last painted; 0 = never.
static LAST_FRAME_MS: AtomicU64 = AtomicU64::new(0);
static WINDOW_CLOSED: AtomicBool = AtomicBool::new(false);
/// Milliseconds since START (+1) at which a close was requested; 0 = not requested.
static CLOSE_REQUESTED_MS: AtomicU64 = AtomicU64::new(0);
/// What the UI thread is doing, shown in the stall report.
static STAGE: Mutex<&'static str> = Mutex::new("starting");
/// A paste request in flight; joined before exit so closing the popup can't drop it.
static PASTE_THREAD: Mutex<Option<thread::JoinHandle<()>>> = Mutex::new(None);

fn elapsed_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn set_stage(stage: &'static str) {
    *STAGE.lock().unwrap_or_else(|e| e.into_inner()) = stage;
}

fn stage() -> &'static str {
    *STAGE.lock().unwrap_or_else(|e| e.into_inner())
}

fn beat() {
    LAST_FRAME_MS.store(elapsed_ms() + 1, Ordering::Relaxed);
}

fn force_exit(code: i32) -> ! {
    store::release_popup_pidfile();
    std::process::exit(code);
}

/// GNOME shows "not responding / Force Quit" when a window's event loop stalls for ~5 s.
/// This thread notices stalls earlier, records what the UI thread was doing, and as a last
/// resort ends the process so a frozen popup can never linger on screen.
fn spawn_watchdog() {
    thread::spawn(|| {
        let mut stalled = false;
        let mut warned_no_frame = false;
        while !WINDOW_CLOSED.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(500));
            let last = LAST_FRAME_MS.load(Ordering::Relaxed);
            let now = elapsed_ms();

            let close_at = CLOSE_REQUESTED_MS.load(Ordering::Relaxed);
            if close_at != 0 && now.saturating_sub(close_at - 1) > 1500 {
                log_error!("watchdog: popup still alive {} ms after a close request (stage: {}); forcing exit", now - (close_at - 1), stage());
                force_exit(0);
            }
            if last == 0 {
                if now > 2500 && !warned_no_frame {
                    warned_no_frame = true;
                    log_warn!("watchdog: no frame painted {now} ms after start (stage: {}); window may be unresponsive", stage());
                }
                continue;
            }
            let silent = now.saturating_sub(last - 1);
            if silent > 5000 {
                log_error!("watchdog: UI thread frozen for {silent} ms (stage: {}); forcing exit", stage());
                force_exit(1);
            } else if silent > 1500 {
                if !stalled {
                    stalled = true;
                    log_warn!("watchdog: UI thread stalled for {silent} ms (stage: {})", stage());
                }
            } else if stalled {
                stalled = false;
                log_info!("watchdog: UI thread recovered");
            }
        }
    });
}

// ------------------------------------------------------------------ data shown in the popup

/// What the popup needs from a history item. Large texts are cut down to a preview once, when
/// the history is loaded, instead of laying out megabytes of text on every frame.
struct Clip {
    id: u64,
    pinned: bool,
    kind: ClipKind,
    /// Lower-cased text for the search box (empty for images).
    haystack: String,
}

enum ClipKind {
    Text { preview: String },
    Image { file: String },
}

fn make_preview(text: &str) -> String {
    const MAX_CHARS: usize = 400;
    const MAX_LINES: usize = 8;
    let mut out = String::new();
    let mut lines = 0;
    for (n, ch) in text.chars().enumerate() {
        if n >= MAX_CHARS || lines >= MAX_LINES {
            out.push('…');
            break;
        }
        if ch == '\n' {
            lines += 1;
        }
        out.push(ch);
    }
    out
}

impl Clip {
    fn from_item(item: store::Item) -> Clip {
        let (kind, haystack) = match item.content {
            Content::Text { text } => (ClipKind::Text { preview: make_preview(&text) }, text.to_lowercase()),
            // Older versions stored no thumbnail; fall back to the full image.
            Content::Image { file, thumb, .. } => (ClipKind::Image { file: if thumb.is_empty() { file } else { thumb } }, String::new()),
        };
        Clip { id: item.id, pinned: item.pinned, kind, haystack }
    }
}

enum Action {
    Close,
    Paste { id: u64, terminal: bool },
    Delete(u64),
    Pin(u64),
    ClearAll,
}

// ------------------------------------------------------------------ app

pub struct Popup {
    pending: Option<mpsc::Receiver<Result<(), String>>>,
    closing: bool,
    pointer: Option<Pos2>,
    monitors: Vec<Rect>,
    /// Where the popup sits, in X screen pixels (None if the window manager chose).
    position: Option<Pos2>,
    x_window: Option<u32>,
    first_frame_done: bool,
    theme: Theme,
    clips: Vec<Clip>,
    /// Indices into `clips` that match the search box, in display order.
    view: Vec<usize>,
    selected: usize,
    query: String,
    textures: HashMap<u64, Option<TextureHandle>>,
    had_focus: bool,
    opened: Instant,
    error: Option<String>,
    scroll_to_selected: bool,
}

pub fn run() -> eframe::Result {
    let t0 = Instant::now();
    START.get_or_init(Instant::now);

    let pointer = x11util::probe_pointer();
    let monitors = x11util::monitors();
    let position = x11util::popup_position(pointer, &monitors, WIN_SIZE);
    log_info!("popup: pointer={pointer:?} monitors={} position={position:?} ({} ms)", monitors.len(), t0.elapsed().as_millis());

    // Everything slow happens here, before a window exists.
    let theme = Theme::detect();
    let fonts = load_fonts();
    log_info!("popup: theme={} fonts ready ({} ms since start)", if theme.dark { "dark" } else { "light" }, t0.elapsed().as_millis());
    spawn_watchdog();

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Clipboard history")
        .with_app_id("winv-clipboard")
        .with_inner_size(WIN_SIZE)
        .with_decorations(false)
        .with_transparent(true)
        .with_resizable(false)
        .with_always_on_top()
        .with_taskbar(false)
        .with_window_type(egui::viewport::X11WindowType::Dialog);
    if let Some(pos) = position {
        viewport = viewport.with_position(pos);
    }
    let options = eframe::NativeOptions { viewport, centered: position.is_none(), ..Default::default() };
    set_stage("creating window");
    let result = eframe::run_native(
        "winv-clipboard",
        options,
        Box::new(move |cc| {
            log_info!("popup: window created ({} ms since start)", t0.elapsed().as_millis());
            cc.egui_ctx.set_fonts(fonts);
            theme.style(&cc.egui_ctx);
            let mut popup = Popup::new(theme, pointer, monitors, position);
            popup.x_window = x_window_id(cc);
            popup.reload();
            log_info!("popup: {} history items loaded", popup.clips.len());
            Ok(Box::new(popup))
        }),
    );
    WINDOW_CLOSED.store(true, Ordering::Relaxed);
    x11util::remove_click_catchers(); // in case the window closed some other way
    // A new popup must be able to start right away, even while a paste request is still in flight.
    store::release_popup_pidfile();
    let paste = PASTE_THREAD.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(handle) = paste {
        let _ = handle.join(); // lets the request reach the service before this process exits
    }
    if let Err(e) = &result {
        log_error!("popup: eframe returned an error: {e}");
    }
    result
}

/// The popup's X11 window id (the popup always runs on X11/XWayland).
fn x_window_id(handle: &impl raw_window_handle::HasWindowHandle) -> Option<u32> {
    use raw_window_handle::RawWindowHandle;
    match handle.window_handle().ok()?.as_raw() {
        RawWindowHandle::Xlib(h) => u32::try_from(h.window).ok(),
        RawWindowHandle::Xcb(h) => Some(h.window.get()),
        _ => None,
    }
}

impl Popup {
    fn new(theme: Theme, pointer: Option<Pos2>, monitors: Vec<Rect>, position: Option<Pos2>) -> Self {
        Popup {
            pending: None,
            closing: false,
            pointer,
            monitors,
            position,
            x_window: None,
            first_frame_done: false,
            theme,
            clips: Vec::new(),
            view: Vec::new(),
            selected: 0,
            query: String::new(),
            textures: HashMap::new(),
            had_focus: false,
            opened: Instant::now(),
            error: None,
            scroll_to_selected: false,
        }
    }

    /// Newest first, pinned items in their place (as in Windows, so Win+V then Enter pastes the
    /// latest copy).
    fn reload(&mut self) {
        let items = match History::load() {
            Ok(history) => history.items,
            Err(e) => {
                log_error!("popup: {e}");
                self.error = Some("The clipboard history could not be read. See `winv-clipboard log errors`.".into());
                Vec::new()
            }
        };
        self.clips = items.into_iter().map(Clip::from_item).collect();
        let alive: Vec<u64> = self.clips.iter().map(|c| c.id).collect();
        self.textures.retain(|id, _| alive.contains(id));
        self.refilter();
    }

    fn refilter(&mut self) {
        let q = self.query.to_lowercase();
        self.view = (0..self.clips.len())
            .filter(|&i| match &self.clips[i].kind {
                ClipKind::Text { .. } => q.is_empty() || self.clips[i].haystack.contains(&q),
                ClipKind::Image { .. } => q.is_empty(),
            })
            .collect();
        self.selected = self.selected.min(self.view.len().saturating_sub(1));
    }

    /// Talks to the service on a worker thread: the UI thread must never wait on it.
    fn apply(&mut self, ctx: &egui::Context, action: Action) {
        if self.closing {
            return; // e.g. a second Enter while the first paste is already on its way
        }
        let (cmd, is_paste) = match action {
            Action::Close => {
                self.request_close(ctx, "close button");
                return;
            }
            Action::Paste { id, terminal: false } => (format!("PASTE {id}"), true),
            Action::Paste { id, terminal: true } => (format!("PASTE_TERMINAL {id}"), true),
            Action::Delete(id) => (format!("DELETE {id}"), false),
            Action::Pin(id) => (format!("PIN {id}"), false),
            Action::ClearAll => ("CLEAR".to_string(), false),
        };
        if self.pending.is_some() {
            return; // the previous request is still running
        }
        log_info!("popup: sending {cmd:?}");
        let (tx, rx) = mpsc::channel();
        let repaint = ctx.clone();
        let handle = thread::spawn(move || {
            let started = Instant::now();
            let result = send(&cmd);
            match &result {
                Ok(()) => log_info!("popup: {cmd:?} acknowledged in {} ms", started.elapsed().as_millis()),
                Err(e) => log_error!("popup: {cmd:?} failed after {} ms: {e}", started.elapsed().as_millis()),
            }
            let _ = tx.send(result);
            repaint.request_repaint();
        });
        if is_paste {
            // Close at once so focus returns to the previous window; `run` waits for the thread.
            *PASTE_THREAD.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
            self.request_close(ctx, "item pasted");
        } else {
            self.pending = Some(rx);
        }
    }

    /// The one place that closes the popup (logged once). Never call this while holding
    /// `ctx.input(..)`: egui's context lock is not re-entrant and the window would hang.
    fn request_close(&mut self, ctx: &egui::Context, why: &str) {
        if !self.closing {
            self.closing = true;
            x11util::remove_click_catchers();
            log_info!("popup: closing ({why})");
            let _ = CLOSE_REQUESTED_MS.compare_exchange(0, elapsed_ms() + 1, Ordering::Relaxed, Ordering::Relaxed);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn poll_pending(&mut self) {
        let Some(rx) = &self.pending else { return };
        match rx.try_recv() {
            Ok(Ok(())) => {
                self.pending = None;
                self.error = None;
                self.reload();
            }
            Ok(Err(e)) => {
                self.pending = None;
                self.error = Some(e);
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => self.pending = None,
        }
    }

    /// Runs once, on the first painted frame, when the real scale factor is known.
    fn first_frame(&mut self, ctx: &egui::Context) {
        self.first_frame_done = true;
        log_info!("popup: first frame painted ({} ms since start)", elapsed_ms());
        let ppp = ctx.pixels_per_point();
        if (ppp - 1.0).abs() > 0.01 {
            // The window manager scales this window: positions were computed for 1:1, so redo them.
            if let Some(pos) = x11util::popup_position(self.pointer, &self.monitors, WIN_SIZE * ppp) {
                self.position = Some(pos);
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition((pos.to_vec2() / ppp).to_pos2()));
            }
        }
        if let Some(origin) = self.position {
            let repaint = ctx.clone();
            let requested = Rect::from_min_size(origin, WIN_SIZE * ppp);
            x11util::spawn_click_catchers(self.x_window, requested, move || repaint.request_repaint());
        }
    }

    fn texture(&mut self, ctx: &egui::Context, id: u64, file: &str) -> Option<TextureHandle> {
        self.textures
            .entry(id)
            .or_insert_with(|| {
                let img = image::open(store::image_path(file)).ok()?.into_rgba8();
                let size = [img.width() as usize, img.height() as usize];
                let color = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
                Some(ctx.load_texture(format!("clip-{id}"), color, egui::TextureOptions::LINEAR))
            })
            .clone()
    }

    /// Keyboard handling. Keys that mean something here are *consumed*, so egui's own focus
    /// handling can't also act on them (Tab moving onto "Clear all" and Enter then pressing it).
    fn handle_keys(&mut self, ctx: &egui::Context) -> Option<Action> {
        let current = self.view.get(self.selected).map(|&i| self.clips[i].id);
        let mut action = None;
        let mut escape = false;
        let mut step: isize = 0;
        let mut typed = String::new();
        let mut backspace = false;
        ctx.input_mut(|i| {
            escape = i.consume_key(Modifiers::NONE, Key::Escape);
            if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                step += 1;
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                step -= 1;
            }
            // Shift+Tab before Tab: matching ignores extra Shift.
            if i.consume_key(Modifiers::SHIFT, Key::Tab) {
                step -= 1;
            } else if i.consume_key(Modifiers::NONE, Key::Tab) {
                step += 1;
            }
            let shift_enter = i.consume_key(Modifiers::SHIFT, Key::Enter);
            let enter = !shift_enter && i.consume_key(Modifiers::NONE, Key::Enter);
            let delete = i.consume_key(Modifiers::NONE, Key::Delete);
            let pin = i.consume_key(Modifiers::CTRL, Key::P);
            backspace = i.consume_key(Modifiers::NONE, Key::Backspace);
            if let Some(id) = current {
                if shift_enter || enter {
                    action = Some(Action::Paste { id, terminal: shift_enter });
                } else if delete {
                    action = Some(Action::Delete(id));
                } else if pin {
                    action = Some(Action::Pin(id));
                }
            }
            // Type-to-search: printable text goes to the filter.
            for ev in &i.events {
                if let egui::Event::Text(t) = ev
                    && !t.chars().any(char::is_control)
                {
                    typed.push_str(t);
                }
            }
        });
        if escape {
            self.request_close(ctx, "Esc");
        }
        if step != 0 && !self.view.is_empty() {
            let count = self.view.len() as isize;
            self.selected = (self.selected as isize + step).rem_euclid(count) as usize;
            self.scroll_to_selected = true;
        }
        if !typed.is_empty() || backspace {
            if backspace {
                self.query.pop();
            }
            self.query.push_str(&typed);
            self.selected = 0;
            self.refilter();
        }
        action
    }
}

impl eframe::App for Popup {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        Color32::TRANSPARENT.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let theme = self.theme;
        set_stage("painting");
        if !self.first_frame_done {
            self.first_frame(&ctx);
        }
        beat();
        self.poll_pending();

        // Close when the user clicks elsewhere, like a flyout. The grace period
        // covers window managers that hand out focus a moment after mapping.
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(false);
        if focused {
            if !self.had_focus {
                log_info!("popup: window focused ({} ms since start)", elapsed_ms());
            }
            self.had_focus = true;
        } else if self.had_focus || self.opened.elapsed() > FOCUS_GRACE {
            let why = if self.had_focus { "lost focus" } else { "never received focus within 3 s" };
            self.request_close(&ctx, why);
        }
        if CLICKED_OUTSIDE.swap(false, Ordering::Relaxed) {
            self.request_close(&ctx, "clicked outside");
        }
        ctx.request_repaint_after(Duration::from_millis(250));

        let mut action = self.handle_keys(&ctx);

        egui::Frame::new()
            .fill(theme.bg)
            .stroke(Stroke::new(1.0, theme.border))
            .corner_radius(CornerRadius::same(8))
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                if let Some(a) = self.header(ui) {
                    action = Some(a);
                }
                if !self.query.is_empty() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("🔍").color(theme.dim));
                        ui.label(RichText::new(&self.query).color(theme.text));
                    });
                }
                if let Some(e) = &self.error {
                    ui.colored_label(Color32::from_rgb(0xff, 0x6b, 0x6b), e);
                }
                let footer_h = 38.0;
                let list_h = ui.available_height() - footer_h;
                egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, false]).show(ui, |ui| {
                    if let Some(a) = self.list(ui, &ctx) {
                        action = Some(a);
                    }
                });
                ui.with_layout(egui::Layout::bottom_up(Align::Center), |ui| {
                    // Bottom line first: this layout stacks upwards.
                    ui.label(RichText::new("Ctrl+P pin    Shift+Enter or Shift+click: paste into a terminal").size(11.0).color(theme.dim));
                    ui.label(RichText::new("↑↓ select    Enter paste    Del delete    Esc close").size(11.0).color(theme.dim));
                });
            });

        if let Some(action) = action {
            self.apply(&ctx, action);
        }
    }
}

impl Popup {
    fn header(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        let theme = self.theme;
        let mut action = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Clipboard history").size(16.0).strong());
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                let close = egui::Button::new(RichText::new("✕").size(14.0).color(theme.dim))
                    .fill(Color32::TRANSPARENT)
                    .min_size(Vec2::new(28.0, 24.0))
                    .corner_radius(CornerRadius::same(4));
                if ui.add(close).on_hover_text("Close (Esc)").clicked() {
                    action = Some(Action::Close);
                }
                let has_unpinned = self.clips.iter().any(|c| !c.pinned);
                let btn = egui::Button::new(RichText::new("Clear all").size(13.0).color(theme.text))
                    .fill(theme.card)
                    .corner_radius(CornerRadius::same(4));
                if ui.add_enabled(has_unpinned, btn).clicked() {
                    action = Some(Action::ClearAll);
                }
            });
        });
        action
    }

    fn list(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) -> Option<Action> {
        let theme = self.theme;
        if self.view.is_empty() {
            ui.add_space(60.0);
            ui.vertical_centered(|ui| {
                let msg = if self.query.is_empty() {
                    "Nothing to see here!\nOnce you copy some text or images, they will show up here."
                } else {
                    "No matching clips."
                };
                ui.label(RichText::new(msg).color(theme.dim).size(14.0));
            });
            return None;
        }

        let mut action = None;
        for position in 0..self.view.len() {
            let index = self.view[position];
            if let Some(a) = self.card(ui, ctx, position, index) {
                action = Some(a);
            }
        }
        action
    }

    /// One history item. `position` is its place in the list, `index` its place in `clips`.
    fn card(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, position: usize, index: usize) -> Option<Action> {
        let theme = self.theme;
        let (id, pinned) = (self.clips[index].id, self.clips[index].pinned);
        let width = ui.available_width() - 4.0;
        let pad = 10.0;
        let inner_w = width - pad * 2.0 - 4.0;

        // Lay the content out first so the card can size itself to it.
        enum Body {
            Text(Arc<egui::Galley>),
            Image(TextureHandle, Vec2),
        }
        let layout = |text: String, color: Color32, wrap: f32, rows: usize| {
            let mut job = LayoutJob::simple(text, FontId::proportional(14.0), color, wrap);
            job.wrap.max_rows = rows;
            job.wrap.break_anywhere = false;
            ctx.fonts_mut(|f| f.layout_job(job))
        };
        let body = match &self.clips[index].kind {
            // Room on the right for the pin and delete buttons.
            ClipKind::Text { preview } => Body::Text(layout(preview.clone(), theme.text, inner_w - 56.0, TEXT_ROWS)),
            ClipKind::Image { file } => {
                let file = file.clone();
                match self.texture(ctx, id, &file) {
                    Some(tex) => {
                        let size = tex.size_vec2();
                        let scale = (inner_w / size.x).min(110.0 / size.y).min(1.0);
                        Body::Image(tex, size * scale)
                    }
                    None => Body::Text(layout("Image unavailable".into(), theme.dim, inner_w, 1)),
                }
            }
        };
        let body_h = match &body {
            Body::Text(g) => g.size().y,
            Body::Image(_, s) => s.y,
        };

        let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, body_h + pad * 2.0), Sense::click());
        if resp.hovered() && ctx.input(|i| i.pointer.delta() != Vec2::ZERO) {
            self.selected = position;
        }
        let selected = self.selected == position;
        if selected && self.scroll_to_selected {
            ui.scroll_to_rect(rect, Some(Align::Center));
            self.scroll_to_selected = false;
        }

        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(6), if selected { theme.card_hover } else { theme.card });
        let stroke = if selected { Stroke::new(1.5, theme.accent) } else { Stroke::new(1.0, theme.border.gamma_multiply(0.5)) };
        painter.rect_stroke(rect, CornerRadius::same(6), stroke, StrokeKind::Inside);

        let content_pos = rect.min + Vec2::splat(pad);
        match &body {
            Body::Text(g) => {
                painter.galley(content_pos, g.clone(), theme.text);
            }
            Body::Image(tex, size) => {
                let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
                painter.image(tex.id(), Rect::from_min_size(content_pos, *size), uv, Color32::WHITE);
            }
        }

        // Pin / delete buttons, visible while hovered or selected (like the "…" menu in Windows).
        let mut action = None;
        let hover_shown = selected || resp.hovered();
        let size = Vec2::splat(22.0);
        let del_rect = Rect::from_min_size(Pos2::new(rect.max.x - 4.0 - size.x, rect.min.y + 4.0), size);
        let pin_rect = del_rect.translate(Vec2::new(-size.x - 2.0, 0.0));
        if hover_shown {
            let r = ui.interact(del_rect, Id::new(("del", id)), Sense::click());
            draw_icon_button(ui, del_rect, "✕", &r, theme, false);
            if r.on_hover_text("Delete").clicked() {
                action = Some(Action::Delete(id));
            }
        }
        if hover_shown || pinned {
            let r = ui.interact(pin_rect, Id::new(("pin", id)), Sense::click());
            draw_icon_button(ui, pin_rect, "📌", &r, theme, pinned);
            if r.on_hover_text(if pinned { "Unpin" } else { "Pin" }).clicked() {
                action = Some(Action::Pin(id));
            }
        }
        if action.is_none() && resp.clicked() {
            let terminal = ctx.input(|i| i.modifiers.shift);
            action = Some(Action::Paste { id, terminal });
        }
        action
    }
}

fn draw_icon_button(ui: &egui::Ui, rect: Rect, glyph: &str, resp: &egui::Response, theme: Theme, active: bool) {
    let painter = ui.painter();
    if resp.hovered() {
        painter.rect_filled(rect, CornerRadius::same(4), theme.border.gamma_multiply(0.8));
    }
    let color = if active { theme.accent } else { theme.dim };
    painter.text(rect.center(), Align2::CENTER_CENTER, glyph, FontId::proportional(13.0), color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_kept_whole() {
        assert_eq!(make_preview("hello\nworld"), "hello\nworld");
    }

    #[test]
    fn long_text_is_cut_with_an_ellipsis() {
        let p = make_preview(&"x".repeat(5000));
        assert_eq!(p.chars().count(), 401);
        assert!(p.ends_with('…'));
    }

    #[test]
    fn many_lines_are_cut_too() {
        let p = make_preview(&"line\n".repeat(50));
        assert!(p.ends_with('…'));
        assert!(p.matches('\n').count() <= 8);
    }

    #[test]
    fn multibyte_text_is_cut_on_character_boundaries() {
        let p = make_preview(&"ગુજરાતી".repeat(200)); // would panic if sliced by bytes
        assert!(p.ends_with('…'));
    }
}
