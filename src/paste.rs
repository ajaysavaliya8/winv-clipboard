//! Sends Ctrl+V (or Ctrl+Shift+V for terminals) to whatever window has focus.
//!
//! GNOME on Wayland does not let ordinary clients inject keys, except through
//! Mutter's RemoteDesktop D-Bus service, which a long-lived session can use
//! without a prompt. X11 windows (XWayland) fall back to the XTEST extension.

use crate::{log_error, log_info, log_warn};
use std::{thread::sleep, time::Duration};
use zbus::{blocking::Connection, zvariant::OwnedObjectPath};

// Linux evdev key codes.
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_V: u32 = 47;

const RD_DEST: &str = "org.gnome.Mutter.RemoteDesktop";
const RD_PATH: &str = "/org/gnome/Mutter/RemoteDesktop";
const RD_SESSION_IFACE: &str = "org.gnome.Mutter.RemoteDesktop.Session";

/// The key chord for a paste, in press order.
fn chord(terminal: bool) -> Vec<u32> {
    if terminal {
        vec![KEY_LEFTCTRL, KEY_LEFTSHIFT, KEY_V]
    } else {
        vec![KEY_LEFTCTRL, KEY_V]
    }
}

/// Presses `keys` in order and always releases whatever was pressed, in reverse order, even if
/// a press failed halfway. A modifier stuck "down" would be far worse than a failed paste.
fn press_chord<E>(keys: &[u32], mut key: impl FnMut(u32, bool) -> Result<(), E>) -> Result<(), E> {
    let mut pressed = 0;
    let mut result = Ok(());
    for &k in keys {
        match key(k, true) {
            Ok(()) => pressed += 1,
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }
    if result.is_ok() {
        sleep(Duration::from_millis(15));
    }
    for &k in keys[..pressed].iter().rev() {
        let _ = key(k, false);
    }
    result
}

struct RemoteDesktop {
    conn: Connection,
    session: OwnedObjectPath,
}

impl RemoteDesktop {
    fn connect() -> zbus::Result<Self> {
        // Bounded calls: a hung call after "Ctrl down" must fail (and so release Ctrl), not block forever.
        let conn = zbus::blocking::connection::Builder::session()?.method_timeout(Duration::from_secs(2)).build()?;
        let session: OwnedObjectPath =
            conn.call_method(Some(RD_DEST), RD_PATH, Some(RD_DEST), "CreateSession", &())?.body().deserialize()?;
        conn.call_method(Some(RD_DEST), session.as_str(), Some(RD_SESSION_IFACE), "Start", &())?;
        Ok(Self { conn, session })
    }

    fn key(&self, code: u32, down: bool) -> zbus::Result<()> {
        self.conn.call_method(
            Some(RD_DEST),
            self.session.as_str(),
            Some(RD_SESSION_IFACE),
            "NotifyKeyboardKeycode",
            &(code, down),
        )?;
        Ok(())
    }

    fn paste(&self, terminal: bool) -> zbus::Result<()> {
        press_chord(&chord(terminal), |code, down| self.key(code, down))
    }
}

#[derive(Default)]
pub struct Paster {
    rd: Option<RemoteDesktop>,
}

impl Paster {
    pub fn paste(&mut self, terminal: bool) {
        let name = if terminal { "Ctrl+Shift+V" } else { "Ctrl+V" };
        for attempt in 0..2 {
            if self.rd.is_none() {
                match RemoteDesktop::connect() {
                    Ok(rd) => {
                        log_info!("RemoteDesktop session opened");
                        self.rd = Some(rd);
                    }
                    Err(e) => {
                        log_warn!("RemoteDesktop unavailable ({e}); falling back to XTEST");
                        break;
                    }
                }
            }
            match self.rd.as_ref().map(|rd| rd.paste(terminal)) {
                Some(Ok(())) => {
                    log_info!("pasted with {name} via RemoteDesktop");
                    return;
                }
                Some(Err(e)) => {
                    log_warn!("RemoteDesktop key injection failed (attempt {attempt}): {e}");
                    self.rd = None; // the session may have expired (e.g. after a screen lock); reconnect once
                }
                None => break,
            }
        }
        match xtest_paste(terminal) {
            Ok(()) => log_info!("pasted with {name} via XTEST"),
            Err(e) => log_error!("could not send {name}: {e}"),
        }
    }
}

/// Fallback for X11 windows. X keycodes are evdev codes + 8.
fn xtest_paste(terminal: bool) -> Result<(), Box<dyn std::error::Error>> {
    use x11rb::{connection::Connection as _, protocol::xtest::ConnectionExt as _};
    const KEY_PRESS: u8 = 2;
    const KEY_RELEASE: u8 = 3;
    let (conn, screen) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen].root;
    press_chord(&chord(terminal), |code, down| -> Result<(), Box<dyn std::error::Error>> {
        let ty = if down { KEY_PRESS } else { KEY_RELEASE };
        conn.xtest_fake_input(ty, (code + 8) as u8, 0, root, 0, 0, 0)?;
        conn.flush()?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chord_releases_in_reverse_order() {
        let mut log = Vec::new();
        press_chord(&[1, 2, 3], |k, down| -> Result<(), ()> {
            log.push((k, down));
            Ok(())
        })
        .unwrap();
        assert_eq!(log, [(1, true), (2, true), (3, true), (3, false), (2, false), (1, false)]);
    }

    #[test]
    fn modifiers_are_released_even_when_a_press_fails() {
        let mut log = Vec::new();
        let result = press_chord(&[10, 20, 30], |k, down| {
            log.push((k, down));
            if k == 30 && down {
                Err("boom")
            } else {
                Ok(())
            }
        });
        assert!(result.is_err());
        // 10 and 20 went down, so both must come back up (20 first).
        assert_eq!(log, [(10, true), (20, true), (30, true), (20, false), (10, false)]);
    }

    #[test]
    fn terminal_chord_adds_shift() {
        assert_eq!(chord(false), [KEY_LEFTCTRL, KEY_V]);
        assert_eq!(chord(true), [KEY_LEFTCTRL, KEY_LEFTSHIFT, KEY_V]);
    }
}
