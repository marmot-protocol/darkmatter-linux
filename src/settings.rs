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
