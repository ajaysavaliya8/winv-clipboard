//! Clipboard history storage shared by the daemon (writer) and the popup (reader).

use crate::{log_error, log_info, log_warn};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

/// Windows keeps the 25 most recent items; pinned items never expire.
pub const MAX_UNPINNED: usize = 25;
/// Windows ignores clips larger than 4 MB.
pub const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum Content {
    Text {
        text: String,
    },
    Image {
        /// Full-size PNG, used when pasting.
        file: String,
        /// Small PNG shown in the popup (empty for items stored by older versions).
        #[serde(default)]
        thumb: String,
        width: usize,
        height: usize,
        hash: u64,
    },
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: u64,
    pub content: Content,
    pub pinned: bool,
    pub ts: u64,
}

#[derive(Default, Serialize, Deserialize)]
pub struct History {
    /// Newest first.
    pub items: Vec<Item>,
    next_id: u64,
    /// Which boot wrote this file; see [`History::load_at_startup`].
    #[serde(default)]
    boot_id: String,
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".local/share")).join("winv-clipboard")
}

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config")).join("winv-clipboard")
}

/// Where the socket and popup pid file live. Never a shared directory like /tmp: another user
/// could pre-create our files there. Every process of this user must agree on it, even one
/// started from a shell without XDG_RUNTIME_DIR, hence the fallbacks.
pub fn runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|d| d.is_dir()) {
        return dir;
    }
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let per_user = PathBuf::from(format!("/run/user/{uid}"));
    if per_user.is_dir() { per_user } else { data_dir() }
}

/// The single-instance lock lives next to the history it protects, so two services can never
/// share one history file, whatever their environment.
pub fn daemon_lock_path() -> PathBuf {
    data_dir().join("daemon.lock")
}

pub fn popup_pidfile() -> PathBuf {
    runtime_dir().join("winv-popup.pid")
}

/// Removes the popup pid file, but only if it is still ours (a newer popup may own it by now).
pub fn release_popup_pidfile() {
    let path = popup_pidfile();
    let ours = fs::read_to_string(&path).map(|s| s.trim() == std::process::id().to_string()).unwrap_or(false);
    if ours {
        let _ = fs::remove_file(path);
    }
}

pub fn image_path(file: &str) -> PathBuf {
    data_dir().join("images").join(file)
}

fn history_path() -> PathBuf {
    data_dir().join("history.json")
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn current_boot_id() -> String {
    fs::read_to_string("/proc/sys/kernel/random/boot_id").map(|s| s.trim().to_string()).unwrap_or_default()
}

/// FNV-1a: stable across runs and Rust versions, unlike `DefaultHasher`.
pub fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// A crash between writing and renaming a save leaves a full copy of the history behind. Only
/// the service writes (and it holds the instance lock), so at its start-up none is in use.
fn remove_stale_temp_files(dir: &std::path::Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with("history.json.tmp.") {
            log_warn!("removing leftover temp file {}", entry.file_name().to_string_lossy());
            let _ = fs::remove_file(entry.path());
        }
    }
}

enum LoadError {
    /// The file exists but is not a history we understand.
    Damaged(String),
    /// The file could not be read at all (permissions, I/O error).
    Unreadable(String),
}

impl History {
    fn read_file() -> Result<History, LoadError> {
        let path = history_path();
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(History::default()),
            Err(e) => return Err(LoadError::Unreadable(format!("cannot read {}: {e}", path.display()))),
        };
        serde_json::from_slice(&bytes).map_err(|e| LoadError::Damaged(format!("{} is damaged: {e}", path.display())))
    }

    /// For the popup: strictly read-only, whatever state the file is in.
    pub fn load() -> Result<History, String> {
        History::read_file().map_err(|(LoadError::Damaged(e) | LoadError::Unreadable(e))| e)
    }

    /// Daemon start-up. Like Windows, history (except pinned items) is cleared when the computer
    /// restarts, because clipboards often hold passwords. Creating `keep-history` in the config
    /// directory turns that off. Restarting only the service keeps everything.
    ///
    /// A damaged file is copied aside before anything is written, and image files are then left
    /// alone, so nothing is lost for good. An unreadable file is an error: overwriting what we
    /// cannot read could destroy it.
    pub fn load_at_startup() -> Result<History, String> {
        let (mut history, intact) = match History::read_file() {
            Ok(history) => (history, true),
            Err(LoadError::Unreadable(e)) => return Err(e),
            Err(LoadError::Damaged(e)) => {
                let backup = history_path().with_extension(format!("json.damaged-{}", now()));
                match fs::copy(history_path(), &backup) {
                    Ok(_) => log_error!("{e}; a copy was saved as {} and the history starts empty", backup.display()),
                    Err(copy_err) => return Err(format!("{e}; and it could not be backed up ({copy_err})")),
                }
                (History::default(), false)
            }
        };
        let boot = current_boot_id();
        if history.boot_id != boot {
            if !history.boot_id.is_empty() && !config_dir().join("keep-history").exists() {
                let dropped = history.items.iter().filter(|i| !i.pinned).count();
                history.clear();
                log_info!("new boot detected: cleared {dropped} unpinned items (pinned items are kept)");
            }
            history.boot_id = boot;
            if let Err(e) = history.save() {
                log_error!("cannot save history: {e}");
            }
        }
        if intact {
            history.remove_orphan_images();
        }
        remove_stale_temp_files(&data_dir());
        Ok(history)
    }

    /// Atomic write (temp file + rename) so the popup never reads a half-written file.
    /// Clipboard contents can be sensitive, so everything is private to the user.
    pub fn save(&self) -> io::Result<()> {
        let dir = data_dir();
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir.join("images"))?;
        let tmp = dir.join(format!("history.json.tmp.{}", std::process::id()));
        let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(&serde_json::to_vec(self)?)?;
        f.sync_all()?;
        fs::rename(tmp, history_path())
    }

    pub fn get(&self, id: u64) -> Option<&Item> {
        self.items.iter().find(|i| i.id == id)
    }

    fn push_front(&mut self, content: Content) {
        self.next_id += 1;
        self.items.insert(0, Item { id: self.next_id, content, pinned: false, ts: now() });
        self.trim();
    }

    pub fn bring_to_top(&mut self, id: u64) {
        if let Some(pos) = self.items.iter().position(|i| i.id == id) {
            let mut item = self.items.remove(pos);
            item.ts = now();
            self.items.insert(0, item);
        }
    }

    fn is_text(item: &Item, text: &str) -> bool {
        matches!(&item.content, Content::Text { text: t } if t == text)
    }

    /// Returns true if the history changed.
    pub fn add_text(&mut self, text: String) -> bool {
        if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
            return false;
        }
        if self.items.first().is_some_and(|i| Self::is_text(i, &text)) {
            return false;
        }
        match self.items.iter().find(|i| Self::is_text(i, &text)).map(|i| i.id) {
            Some(existing) => self.bring_to_top(existing),
            None => self.push_front(Content::Text { text }),
        }
        true
    }

    pub fn find_image(&self, hash: u64) -> Option<u64> {
        self.items.iter().find(|i| matches!(i.content, Content::Image { hash: h, .. } if h == hash)).map(|i| i.id)
    }

    /// `png` is the full image, `thumb` the popup preview; `hash` identifies the raw pixels.
    pub fn add_image(&mut self, png: &[u8], thumb: &[u8], width: usize, height: usize, hash: u64) -> io::Result<bool> {
        if let Some(id) = self.find_image(hash) {
            if self.items.first().is_some_and(|i| i.id == id) {
                return Ok(false);
            }
            self.bring_to_top(id);
            return Ok(true);
        }
        if png.is_empty() || thumb.is_empty() {
            // The caller skipped encoding because the image was known, but it has been deleted
            // in the meantime. Recording it now would create an item without a file.
            return Ok(false);
        }
        fs::DirBuilder::new().recursive(true).mode(0o700).create(data_dir().join("images"))?;
        let write = |name: &str, bytes: &[u8]| -> io::Result<()> {
            fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(image_path(name))?.write_all(bytes)
        };
        let (file, thumb_file) = (format!("{hash:016x}.png"), format!("{hash:016x}.thumb.png"));
        write(&file, png)?;
        write(&thumb_file, thumb)?;
        self.push_front(Content::Image { file, thumb: thumb_file, width, height, hash });
        Ok(true)
    }

    pub fn delete(&mut self, id: u64) {
        if let Some(pos) = self.items.iter().position(|i| i.id == id) {
            let item = self.items.remove(pos);
            self.remove_image_files(&item);
        }
    }

    /// Pinning never moves an item. Unpinning doesn't delete anything right away either (even an
    /// item older than the 25 most recent): it simply expires with the next copy, like any other
    /// item, so an accidental unpin can be undone by pinning again.
    pub fn toggle_pin(&mut self, id: u64) {
        if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
            item.pinned = !item.pinned;
        }
    }

    /// "Clear all" in Windows keeps pinned items.
    pub fn clear(&mut self) {
        let (keep, drop): (Vec<_>, Vec<_>) = std::mem::take(&mut self.items).into_iter().partition(|i| i.pinned);
        self.items = keep;
        for item in drop {
            self.remove_image_files(&item);
        }
    }

    fn trim(&mut self) {
        let mut seen = 0;
        let mut removed = Vec::new();
        self.items.retain(|i| {
            if i.pinned {
                return true;
            }
            seen += 1;
            if seen > MAX_UNPINNED {
                removed.push(i.clone());
                false
            } else {
                true
            }
        });
        for item in removed {
            self.remove_image_files(&item);
        }
    }

    fn remove_image_files(&self, item: &Item) {
        if let Content::Image { file, thumb, hash, .. } = &item.content {
            // Only delete the files if no other item shares them.
            if !self.items.iter().any(|i| matches!(i.content, Content::Image { hash: h, .. } if h == *hash)) {
                let _ = fs::remove_file(image_path(file));
                if !thumb.is_empty() {
                    let _ = fs::remove_file(image_path(thumb));
                }
            }
        }
    }

    /// Deletes image files no item refers to (left behind by a crash between write and save).
    fn remove_orphan_images(&self) {
        let used: Vec<&str> = self
            .items
            .iter()
            .filter_map(|i| match &i.content {
                Content::Image { file, thumb, .. } => Some([file.as_str(), thumb.as_str()]),
                Content::Text { .. } => None,
            })
            .flatten()
            .collect();
        let Ok(entries) = fs::read_dir(data_dir().join("images")) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !used.contains(&name.as_str()) {
                log_warn!("removing orphaned image file {name}");
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(h: &History) -> Vec<&str> {
        h.items
            .iter()
            .map(|i| match &i.content {
                Content::Text { text } => text.as_str(),
                Content::Image { .. } => "<image>",
            })
            .collect()
    }

    #[test]
    fn newest_first_and_duplicates_move_to_top() {
        let mut h = History::default();
        assert!(h.add_text("a".into()));
        assert!(h.add_text("b".into()));
        assert!(!h.add_text("b".into()), "same as the newest item is a no-op");
        assert!(h.add_text("a".into()), "an older duplicate moves to the top");
        assert_eq!(text(&h), ["a", "b"]);
    }

    #[test]
    fn blank_and_oversized_text_is_ignored() {
        let mut h = History::default();
        assert!(!h.add_text("  \n\t".into()));
        assert!(!h.add_text("x".repeat(MAX_TEXT_BYTES + 1)));
        assert!(h.add_text("x".repeat(MAX_TEXT_BYTES)));
    }

    #[test]
    fn keeps_25_recent_items_but_never_drops_pinned_ones() {
        let mut h = History::default();
        h.add_text("keep me".into());
        let id = h.items[0].id;
        h.toggle_pin(id);
        for n in 0..40 {
            h.add_text(format!("item {n}"));
        }
        let unpinned = h.items.iter().filter(|i| !i.pinned).count();
        assert_eq!(unpinned, MAX_UNPINNED);
        assert!(h.get(id).is_some_and(|i| i.pinned));
        assert_eq!(text(&h)[0], "item 39");
    }

    #[test]
    fn clear_all_keeps_pinned_items() {
        let mut h = History::default();
        h.add_text("pinned".into());
        let id = h.items[0].id;
        h.toggle_pin(id);
        h.add_text("gone".into());
        h.clear();
        assert_eq!(text(&h), ["pinned"]);
    }

    #[test]
    fn delete_and_bring_to_top() {
        let mut h = History::default();
        for t in ["a", "b", "c"] {
            h.add_text(t.into());
        }
        let a = h.items.iter().find(|i| History::is_text(i, "a")).unwrap().id;
        h.bring_to_top(a);
        assert_eq!(text(&h), ["a", "c", "b"]);
        h.delete(a);
        assert_eq!(text(&h), ["c", "b"]);
    }

    #[test]
    fn ids_are_never_reused() {
        let mut h = History::default();
        h.add_text("a".into());
        let first = h.items[0].id;
        h.delete(first);
        h.add_text("b".into());
        assert_ne!(h.items[0].id, first);
    }

    #[test]
    fn unpinning_an_old_item_does_not_delete_it_and_can_be_undone() {
        let mut h = History::default();
        h.add_text("old favourite".into());
        let id = h.items[0].id;
        h.toggle_pin(id);
        for n in 0..30 {
            h.add_text(format!("item {n}"));
        }
        h.toggle_pin(id); // accidental unpin
        assert!(h.get(id).is_some(), "still there right after unpinning");
        h.toggle_pin(id); // undo
        h.add_text("one more copy".into());
        assert!(h.get(id).is_some_and(|i| i.pinned), "re-pinned item survives later copies");
    }

    #[test]
    fn an_unpinned_old_item_expires_with_the_next_copy() {
        let mut h = History::default();
        h.add_text("old".into());
        let id = h.items[0].id;
        h.toggle_pin(id);
        for n in 0..30 {
            h.add_text(format!("item {n}"));
        }
        h.toggle_pin(id);
        h.add_text("new copy".into());
        assert!(h.get(id).is_none());
        assert_eq!(h.items.len(), MAX_UNPINNED);
    }

    #[test]
    fn an_image_without_encoded_files_is_never_recorded() {
        // The daemon skips encoding for an image it already has; if that item was deleted in
        // the meantime, nothing must be recorded (no disk access happens on this path).
        let mut h = History::default();
        assert!(!h.add_image(&[], &[], 10, 10, 42).unwrap());
        assert!(h.items.is_empty());
    }

    #[test]
    fn leftover_save_files_are_removed_but_nothing_else() {
        let dir = std::env::temp_dir().join(format!("winv-store-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        for name in ["history.json", "history.json.tmp.4242", "daemon.lock"] {
            fs::write(dir.join(name), "x").unwrap();
        }
        remove_stale_temp_files(&dir);
        let mut left: Vec<String> = fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(left, ["daemon.lock", "history.json"]);
    }

    #[test]
    fn history_written_by_an_older_version_still_loads() {
        // No `thumb` on images and no `boot_id` on the history.
        let json = r#"{"items":[{"id":1,"content":{"kind":"Image","file":"a.png","width":2,"height":2,"hash":7},"pinned":false,"ts":0}],"next_id":1}"#;
        let h: History = serde_json::from_str(json).unwrap();
        assert!(matches!(&h.items[0].content, Content::Image { thumb, .. } if thumb.is_empty()));
    }
}
