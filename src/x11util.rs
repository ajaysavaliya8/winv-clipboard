//! Helpers that talk to the X server (XWayland on GNOME/Wayland) to place the popup
//! at the mouse pointer and to notice clicks outside of it.
//!
//! Wayland deliberately hides the pointer from applications that have no window under it,
//! so the tricks here use short-lived, invisible, override-redirect windows (the window
//! manager never moves, resizes or restacks those).

use crate::{log_info, log_warn};
use eframe::egui::{Pos2, Rect, Vec2};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::{randr::ConnectionExt as _, xproto::*, Event},
    rust_connection::RustConnection,
};

/// Monitor rectangles in X screen pixels (empty if the X server can't tell).
pub fn monitors() -> Vec<Rect> {
    let Ok((conn, screen_num)) = x11rb::connect(None) else { return Vec::new() };
    let root = conn.setup().roots[screen_num].root;
    let Some(reply) = conn.randr_get_monitors(root, true).ok().and_then(|c| c.reply().ok()) else { return Vec::new() };
    reply
        .monitors
        .iter()
        .map(|m| Rect::from_min_size(Pos2::new(m.x as f32, m.y as f32), Vec2::new(m.width as f32, m.height as f32)))
        .collect()
}

/// Creates (unmapped) a fully transparent override-redirect window.
fn transparent_window(conn: &impl Connection, screen: &Screen, rect: (i16, i16, u16, u16), events: EventMask) -> Option<Window> {
    let visual = screen
        .allowed_depths
        .iter()
        .find(|d| d.depth == 32)
        .and_then(|d| d.visuals.iter().find(|v| v.class == VisualClass::TRUE_COLOR))?
        .visual_id;
    let colormap = conn.generate_id().ok()?;
    conn.create_colormap(ColormapAlloc::NONE, colormap, screen.root, visual).ok()?;
    let win = conn.generate_id().ok()?;
    conn.create_window(
        32,
        win,
        screen.root,
        rect.0,
        rect.1,
        rect.2,
        rect.3,
        0,
        WindowClass::INPUT_OUTPUT,
        visual,
        &CreateWindowAux::new()
            .background_pixel(0) // fully transparent
            .border_pixel(0)
            .colormap(colormap)
            .override_redirect(1)
            .event_mask(events),
    )
    .ok()?;
    Some(win)
}

/// Finds the mouse pointer: briefly maps an invisible window over the whole desktop and
/// reads where the pointer enters it. `None` if nothing arrives within 400 ms.
pub fn probe_pointer() -> Option<Pos2> {
    let (conn, screen_num) = x11rb::connect(None).ok()?;
    let screen = &conn.setup().roots[screen_num];
    let size = (screen.width_in_pixels, screen.height_in_pixels);
    let win = transparent_window(&conn, screen, (0, 0, size.0, size.1), EventMask::ENTER_WINDOW | EventMask::POINTER_MOTION)?;
    conn.map_window(win).ok()?;
    conn.flush().ok()?;

    let deadline = Instant::now() + Duration::from_millis(400);
    let mut found = None;
    while found.is_none() && Instant::now() < deadline {
        match conn.poll_for_event().ok()? {
            Some(Event::EnterNotify(e)) => found = Some(Pos2::new(e.root_x as f32, e.root_y as f32)),
            Some(Event::MotionNotify(e)) => found = Some(Pos2::new(e.root_x as f32, e.root_y as f32)),
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(4)),
        }
    }
    let _ = conn.destroy_window(win);
    let _ = conn.flush();
    found
}

/// Where the popup's top-left corner goes: just below-right of the pointer, kept inside the
/// monitor that holds the pointer (flipping above it near the bottom edge). With no pointer
/// the popup is centred on the first monitor; with no monitors, `None` (let the WM decide).
pub fn popup_position(pointer: Option<Pos2>, monitors: &[Rect], size: Vec2) -> Option<Pos2> {
    const MARGIN: f32 = 8.0;
    const TOP_BAR: f32 = 36.0; // keep clear of GNOME's top bar
    let Some(anchor) = pointer else { return monitors.first().map(|m| m.center() - size / 2.0) };
    let monitor = monitors.iter().find(|m| m.contains(anchor)).or(monitors.first())?;
    let area = Rect::from_min_max(
        Pos2::new(monitor.min.x + MARGIN, monitor.min.y + TOP_BAR),
        Pos2::new(monitor.max.x - MARGIN, monitor.max.y - MARGIN),
    );
    let mut pos = anchor + Vec2::splat(4.0);
    if pos.x + size.x > area.max.x {
        pos.x = area.max.x - size.x;
    }
    if pos.y + size.y > area.max.y {
        pos.y = anchor.y - size.y - 4.0;
    }
    Some(Pos2::new(pos.x.max(area.min.x), pos.y.max(area.min.y)))
}

/// Set by the click catchers when the user clicks anywhere outside the popup.
pub static CLICKED_OUTSIDE: AtomicBool = AtomicBool::new(false);

/// The live catcher windows, so they can be removed the moment the popup starts closing (the
/// process may live on briefly to finish a paste request, and must not keep eating clicks).
struct Catchers {
    removed: bool,
    windows: Option<(Arc<RustConnection>, Vec<Window>)>,
}
static CATCHERS: Mutex<Catchers> = Mutex::new(Catchers { removed: false, windows: None });

/// Removes the click catchers now, or prevents them from appearing if they are still being set up.
pub fn remove_click_catchers() {
    let mut catchers = CATCHERS.lock().unwrap_or_else(|e| e.into_inner());
    catchers.removed = true;
    if let Some((conn, windows)) = catchers.windows.take() {
        for w in &windows {
            let _ = conn.destroy_window(*w);
        }
        let _ = conn.flush();
        log_info!("popup: {} click-outside catchers removed", windows.len());
    }
}

/// Focus changes don't cover every outside click (the desktop background and GNOME's top bar
/// never take focus), so frame the popup with four invisible windows (above, below, left and
/// right of it) that report any click. Clicks inside the popup reach the popup as usual.
///
/// `window` is the popup's X window: its real on-screen rectangle is used, so the catchers can
/// never overlap the popup even if the window manager placed it differently than requested.
/// `requested` is the fallback when the window can't be queried.
pub fn spawn_click_catchers(window: Option<u32>, requested: Rect, repaint: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        if let Err(e) = run_click_catchers(window, requested, repaint) {
            log_warn!("popup: click-outside catchers unavailable ({e}); relying on focus loss");
        }
    });
}

/// The window's rectangle in root coordinates, once it is mapped and has stopped moving.
fn settled_geometry(conn: &impl Connection, root: Window, window: Window) -> Option<Rect> {
    let read = || -> Option<Rect> {
        if conn.get_window_attributes(window).ok()?.reply().ok()?.map_state != MapState::VIEWABLE {
            return None;
        }
        let geo = conn.get_geometry(window).ok()?.reply().ok()?;
        let pos = conn.translate_coordinates(window, root, 0, 0).ok()?.reply().ok()?;
        Some(Rect::from_min_size(Pos2::new(pos.dst_x as f32, pos.dst_y as f32), Vec2::new(geo.width as f32, geo.height as f32)))
    };
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut last = None;
    while Instant::now() < deadline {
        let now = read();
        if now.is_some() && now == last {
            return now;
        }
        last = now;
        std::thread::sleep(Duration::from_millis(40));
    }
    last
}

fn run_click_catchers(window: Option<u32>, requested: Rect, repaint: impl Fn()) -> Result<(), Box<dyn std::error::Error>> {
    let (conn, screen_num) = x11rb::connect(None)?;
    let conn = Arc::new(conn);
    let screen = &conn.setup().roots[screen_num];
    let actual = window.and_then(|w| settled_geometry(&*conn, screen.root, w));
    let panel = match actual {
        Some(rect) => {
            if (rect.min - requested.min).length() > 2.0 {
                log_warn!("popup: window manager placed the popup at {:?} instead of {:?}", rect.min, requested.min);
            }
            rect
        }
        None => requested,
    };
    let (w, h) = (screen.width_in_pixels as i32, screen.height_in_pixels as i32);
    let (x0, y0) = ((panel.min.x as i32).clamp(0, w), (panel.min.y as i32).clamp(0, h));
    let (x1, y1) = ((panel.max.x.ceil() as i32).clamp(0, w), (panel.max.y.ceil() as i32).clamp(0, h));
    let strips = [(0, 0, w, y0), (0, y1, w, h - y1), (0, y0, x0, y1 - y0), (x1, y0, w - x1, y1 - y0)];
    {
        // Holding the lock while creating means `remove_click_catchers` either runs before
        // (and we create nothing) or after (and it sees every window).
        let mut catchers = CATCHERS.lock().unwrap_or_else(|e| e.into_inner());
        if catchers.removed {
            return Ok(());
        }
        let mut windows = Vec::new();
        for (x, y, sw, sh) in strips {
            if sw <= 0 || sh <= 0 {
                continue;
            }
            let win = transparent_window(&*conn, screen, (x as i16, y as i16, sw as u16, sh as u16), EventMask::BUTTON_PRESS)
                .ok_or("cannot create a transparent window")?;
            conn.map_window(win)?;
            windows.push(win);
        }
        conn.flush()?;
        log_info!("popup: {} click-outside catchers active around {panel:?}", windows.len());
        catchers.windows = Some((conn.clone(), windows));
    }
    loop {
        // Buttons 1-3 are real clicks; 4-7 are the scroll wheel.
        if let Event::ButtonPress(e) = conn.wait_for_event()?
            && e.detail <= 3
        {
            log_info!("popup: click outside (button {}) at {},{}", e.detail, e.root_x, e.root_y);
            CLICKED_OUTSIDE.store(true, Ordering::Relaxed);
            repaint();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: Vec2 = Vec2::new(372.0, 520.0);

    fn monitor(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h))
    }

    #[test]
    fn opens_just_below_right_of_the_pointer() {
        let m = [monitor(0.0, 0.0, 1920.0, 1080.0)];
        let pos = popup_position(Some(Pos2::new(500.0, 200.0)), &m, SIZE).unwrap();
        assert_eq!(pos, Pos2::new(504.0, 204.0));
    }

    #[test]
    fn stays_inside_the_monitor_near_the_right_and_bottom_edges() {
        let m = [monitor(0.0, 0.0, 1920.0, 1080.0)];
        let pos = popup_position(Some(Pos2::new(1900.0, 1070.0)), &m, SIZE).unwrap();
        assert!(pos.x + SIZE.x <= 1920.0 - 8.0, "{pos:?}");
        assert!(pos.y + SIZE.y <= 1080.0, "{pos:?}");
        assert!(pos.y < 1070.0, "flips above the pointer: {pos:?}");
    }

    #[test]
    fn uses_the_monitor_that_holds_the_pointer() {
        let m = [monitor(0.0, 0.0, 1080.0, 1920.0), monitor(1080.0, 503.0, 1920.0, 1080.0), monitor(3000.0, 40.0, 1080.0, 1920.0)];
        let pos = popup_position(Some(Pos2::new(3500.0, 900.0)), &m, SIZE).unwrap();
        assert!(pos.x >= 3000.0 && pos.x + SIZE.x <= 4080.0, "{pos:?}");
    }

    #[test]
    fn never_covers_the_top_bar() {
        let m = [monitor(0.0, 0.0, 1920.0, 1080.0)];
        let pos = popup_position(Some(Pos2::new(100.0, 2.0)), &m, SIZE).unwrap();
        assert!(pos.y >= 36.0, "{pos:?}");
    }

    #[test]
    fn without_a_pointer_it_is_centred_and_without_monitors_left_to_the_wm() {
        let m = [monitor(0.0, 0.0, 1920.0, 1080.0)];
        assert_eq!(popup_position(None, &m, SIZE).unwrap(), Pos2::new(774.0, 280.0));
        assert!(popup_position(Some(Pos2::new(10.0, 10.0)), &[], SIZE).is_none());
    }
}
