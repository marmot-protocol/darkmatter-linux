// App-level settings persisted as a tiny JSON file in XDG_CONFIG_HOME.
// Stores UI prefs such as locale, theme, accent color, and debug toggle.
// Failures (read, parse, write) are swallowed — defaults keep the app
// booting even if the config file is corrupt or missing.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub debug_enabled: bool,
    /// BCP-47 language tag without region, e.g. `en`, `it`, `de`, `ja`.
    #[serde(default = "default_locale")]
    pub locale: String,
    /// One of `dark`, `light`, or `retro`.
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Accent color name. One of `mint`, `ocean`, `berry`, `coral`, `lavender`.
    #[serde(default = "default_accent")]
    pub accent_color: String,
    #[serde(default = "default_outgoing_on_right")]
    pub outgoing_on_right: bool,
    /// Clock style for all visible timestamps. One of `24h`, `12h`.
    #[serde(default = "default_time_format")]
    pub time_format: String,
    /// Date style for all visible stamps. One of `mdy` ("Jun 12"),
    /// `dmy` ("12 Jun"), `iso` ("2026-06-12").
    #[serde(default = "default_date_format")]
    pub date_format: String,
    /// Private per-contact nicknames, keyed by the contact's account id (hex).
    /// Local-only — never published to relays.
    #[serde(default)]
    pub nicknames: BTreeMap<String, String>,
    /// Fire a desktop notification for incoming messages in chats you aren't
    /// currently viewing. Master switch for the two below.
    #[serde(default = "default_true")]
    pub notifications_enabled: bool,
    /// Ask the notification server to play its message sound.
    #[serde(default = "default_true")]
    pub notification_sound: bool,
    /// Include the message text in the notification body (off = "New message").
    #[serde(default = "default_true")]
    pub notification_preview: bool,
    /// Chats (group_id_hex) the user has muted — suppresses their desktop
    /// notifications. Local-only, like nicknames.
    #[serde(default)]
    pub muted_chats: BTreeSet<String>,
    /// Per-chat read marker: `group_id_hex` → the Unix-seconds timestamp the
    /// user last viewed that chat. Messages recorded after the marker count as
    /// unread. Written when a chat is opened; the authoritative read state the
    /// rail/tray unread counts derive from. Local-only, like nicknames.
    #[serde(default)]
    pub last_read: BTreeMap<String, i64>,
    /// Unsent composer text ("drafts"), keyed by `group_id_hex`. Written when
    /// the user switches away from (or quits with) a half-written message, and
    /// restored when the chat is reopened. Local-only, like nicknames — never
    /// published to relays.
    #[serde(default)]
    pub composer_drafts: BTreeMap<String, String>,
    /// Messages the user deleted *for themselves* ("delete for me"), keyed by
    /// the local account hex that hid them → the set of inner event ids (hex).
    /// Local-only — never published; the message stays on the wire for everyone
    /// else, it's just filtered out of this client's view. Per-account so a hide
    /// on one account doesn't leak to another account on the same machine.
    #[serde(default)]
    pub hidden_messages_by_account: BTreeMap<String, BTreeSet<String>>,
    /// Legacy global hidden set (pre per-account scoping). The old `hidden_messages`
    /// key deserializes here; at boot it's folded into the boot account's
    /// in-memory hidden set so pre-upgrade "delete for me" hides survive. Kept in
    /// the file (not account-attributed on disk) since it predates account scoping.
    #[serde(default, rename = "hidden_messages")]
    pub hidden_messages_legacy: BTreeSet<String>,
}

impl Settings {
    /// Hide `message_id` for `account_hex`. Returns true if it wasn't already
    /// hidden (so the caller knows to persist).
    pub fn hide_message(&mut self, account_hex: &str, message_id: &str) -> bool {
        self.hidden_messages_by_account
            .entry(account_hex.to_ascii_lowercase())
            .or_default()
            .insert(message_id.to_string())
    }

    /// Store (or clear) the unsent draft for `group_hex`. Whitespace-only text
    /// drops the entry so an emptied composer leaves nothing behind. Returns
    /// true when the stored value actually changed, so callers only `save()`
    /// (a disk write) when there's something new to persist.
    pub fn set_draft(&mut self, group_hex: &str, text: &str) -> bool {
        if text.trim().is_empty() {
            self.composer_drafts.remove(group_hex).is_some()
        } else {
            self.composer_drafts
                .insert(group_hex.to_string(), text.to_string())
                .as_deref()
                != Some(text)
        }
    }

    /// The saved draft for `group_hex`, or `""` if none.
    pub fn draft(&self, group_hex: &str) -> &str {
        self.composer_drafts
            .get(group_hex)
            .map(String::as_str)
            .unwrap_or_default()
    }
}

fn default_locale() -> String {
    "en".into()
}

fn default_theme() -> String {
    "dark".into()
}

fn default_accent() -> String {
    "mint".into()
}

fn default_outgoing_on_right() -> bool {
    true
}

fn default_time_format() -> String {
    "24h".into()
}

fn default_date_format() -> String {
    "mdy".into()
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            debug_enabled: false,
            locale: default_locale(),
            theme: default_theme(),
            accent_color: default_accent(),
            outgoing_on_right: default_outgoing_on_right(),
            time_format: default_time_format(),
            date_format: default_date_format(),
            nicknames: BTreeMap::new(),
            notifications_enabled: true,
            notification_sound: true,
            notification_preview: true,
            muted_chats: BTreeSet::new(),
            last_read: BTreeMap::new(),
            composer_drafts: BTreeMap::new(),
            hidden_messages_by_account: BTreeMap::new(),
            hidden_messages_legacy: BTreeSet::new(),
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        let path = match Self::path() {
            Some(p) => p,
            None => return Self::default(),
        };
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(_) => return Self::default(),
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    pub fn save(&self) {
        let path = match Self::path() {
            Some(p) => p,
            None => return,
        };
        if let Some(parent) = path.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            eprintln!("[settings] create_dir_all({}): {e}", parent.display());
            return;
        }
        let bytes = match serde_json::to_vec_pretty(self) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[settings] serialize: {e}");
                return;
            }
        };
        if let Err(e) = fs::write(&path, bytes) {
            eprintln!("[settings] write({}): {e}", path.display());
        }
    }

    fn path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "darkmatter-linux")
            .map(|d| d.config_dir().join("settings.json"))
    }
}
