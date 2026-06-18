// Desktop notifications for incoming messages.
//
// Two pieces:
//   * `NotifState` — the shared, `Send`/`Sync` runtime state read from the chat
//     watcher (which fires on the tokio thread and hops to the UI thread via
//     `invoke_from_event_loop`, so an `Rc<RefCell<Settings>>` can't cross the
//     boundary — atomics + a mutex can). It holds the three user toggles plus a
//     `seen` map used to tell genuine new arrivals apart from the backlog.
//   * `show` — fires one OS notification. Does dbus IO on Linux, so callers run
//     it off the UI thread.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

/// Shared notification state. One instance lives for the whole process behind an
/// `Arc`; the toggle callbacks write the atomics and the chat watcher reads them.
pub struct NotifState {
    pub enabled: AtomicBool,
    pub sound: AtomicBool,
    pub preview: AtomicBool,
    /// group_id_hex → the id of the latest message we've already notified for.
    /// Dedupes repeat watcher fires on the same chat: a reaction/edit/receipt
    /// changes the group fingerprint (so the watcher refires) but leaves the
    /// latest message id unchanged, and must not re-notify. Boot/backlog
    /// suppression is handled separately by a message-recency gate at the call
    /// site, not here.
    seen: Mutex<HashMap<String, String>>,
    /// group_id_hex of chats the user muted — their incoming messages never
    /// notify. Mirrors `Settings::muted_chats`; the toggle callback writes both.
    muted: Mutex<BTreeSet<String>>,
}

impl NotifState {
    pub fn new(enabled: bool, sound: bool, preview: bool, muted: BTreeSet<String>) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            sound: AtomicBool::new(sound),
            preview: AtomicBool::new(preview),
            seen: Mutex::new(HashMap::new()),
            muted: Mutex::new(muted),
        }
    }

    pub fn is_muted(&self, group_hex: &str) -> bool {
        match self.muted.lock() {
            Ok(g) => g.contains(group_hex),
            Err(p) => p.into_inner().contains(group_hex),
        }
    }

    pub fn set_muted(&self, group_hex: &str, muted: bool) {
        let mut g = match self.muted.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if muted {
            g.insert(group_hex.to_string());
        } else {
            g.remove(group_hex);
        }
    }

    /// Record the latest message id for a group and report whether it changed
    /// since the last call — i.e. whether this is a not-yet-notified message.
    /// Returns `false` only when the id is unchanged (a refire on the same
    /// latest message). A first sighting or a changed id returns `true`; the
    /// caller's recency gate decides whether a `true` is actually fresh enough
    /// to notify.
    pub fn note_latest(&self, group_hex: &str, latest_message_id: &str) -> bool {
        let mut seen = match self.seen.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let unchanged = seen.get(group_hex).map(String::as_str) == Some(latest_message_id);
        if !unchanged {
            seen.insert(group_hex.to_string(), latest_message_id.to_string());
        }
        !unchanged
    }
}

/// Fire one desktop notification. Best-effort: a failing notification server
/// just logs. `play_sound` asks the server to play its standard message sound
/// (freedesktop sound naming) rather than bundling an audio asset.
pub fn show(summary: &str, body: &str, play_sound: bool) {
    let mut n = notify_rust::Notification::new();
    n.appname("darkmatter").summary(summary).body(body);
    if play_sound {
        n.sound_name("message-new-instant");
    }
    if let Err(e) = n.show() {
        eprintln!("[notify] show failed: {e}");
    }
}
