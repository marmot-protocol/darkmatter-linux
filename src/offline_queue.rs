// Durable offline send queue.
//
// Messages composed without connectivity must survive an app restart and go out
// automatically once a relay is reachable again. marmot's `send_message` is
// transactional — when the relay publish fails it rolls the MLS ratchet back
// within the same call and returns an error, leaving nothing persisted — so it
// is *safe* to re-dispatch a failed send later without corrupting group state
// or double-advancing the ratchet. The only thing missing is durability: the
// optimistic overlay (`PendingState` in `main.rs`) lives in RAM and is lost on
// exit. This module is that missing piece.
//
// Each queued send is sealed with the vault key (XChaCha20-Poly1305, via
// `Vault::seal_blob`, the same path the media cache uses) and written to its own
// file at `$DM_HOME/offline-queue/<temp_id_hex>.bin` (mode 0600). One file per
// entry — not one big blob — so acking a send deletes a single small file
// instead of re-sealing every still-queued attachment's bytes. Message bodies
// and attachment bytes are user content, so nothing here ever touches disk in
// the clear, matching the rest of the app's at-rest-encryption guarantee.
//
// The store is best-effort: any IO/crypto failure is logged and swallowed. A
// failure to persist degrades to today's behaviour (in-RAM only, lost on
// restart) rather than blocking the send.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::vault::Vault;

/// One attachment's worth of bytes needed to re-upload a queued media send.
/// Unlike the UI-side `PendingMedia` (which only keeps decoded preview pixels),
/// this carries the *original* compressed bytes so the upload can be retried
/// after a restart.
#[derive(Clone, Serialize, Deserialize)]
pub struct QueuedMedia {
    pub file_name: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
    pub is_image: bool,
}

/// What kind of send this is — mirrors the three dispatch paths in `main.rs`
/// (`dispatch_send`, `spawn_attachment_send`, `spawn_album_send`).
#[derive(Clone, Serialize, Deserialize)]
pub enum QueuedKind {
    /// Plain text or a reply. `reply_to` is the (parent_id, author_label,
    /// preview) triple carried by `PendingSend::reply_to`; `effect` is the armed
    /// message-effect id (0 = none).
    Text {
        text: String,
        reply_to: Option<(String, String, String)>,
        effect: i32,
    },
    /// A single attachment (image or file).
    Attachment(QueuedMedia),
    /// A multi-image album sent as one kind-9.
    Album(Vec<QueuedMedia>),
}

/// A single durable entry. `temp_id` is the same local id the in-RAM overlay
/// uses (carried into the bubble's `message_id`), so the disk entry and the
/// overlay entry resolve to each other. `account_id_hex` scopes the entry to one
/// account (the vault is shared across accounts); `group_hex` is the target
/// group. `enqueued_at` (unix seconds) drives the boot-time duplicate check.
#[derive(Clone, Serialize, Deserialize)]
pub struct QueuedSend {
    pub temp_id: String,
    pub account_id_hex: String,
    pub group_hex: String,
    pub kind: QueuedKind,
    pub enqueued_at: u64,
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn queue_dir() -> PathBuf {
    crate::backend::default_home().join("offline-queue")
}

/// Filesystem-safe filename for a temp id. The id format (`pending:N`) carries a
/// colon, which is fine on unix but not on every filesystem, so hex-encode it.
fn path_for(temp_id: &str) -> PathBuf {
    queue_dir().join(format!("{}.bin", hex::encode(temp_id.as_bytes())))
}

/// Seal `send` under the vault key and write it durably. Best-effort: failures
/// are logged and swallowed, degrading to in-RAM-only behaviour.
pub fn put(vault: &Arc<Mutex<Vault>>, send: &QueuedSend) {
    let plaintext = match serde_json::to_vec(send) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[offline-queue] encode {}: {e}", send.temp_id);
            return;
        }
    };
    let sealed = {
        let Ok(v) = vault.lock() else { return };
        match v.seal_blob(&plaintext) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[offline-queue] seal {}: {e}", send.temp_id);
                return;
            }
        }
    };
    let dir = queue_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("[offline-queue] mkdir: {e}");
        return;
    }
    set_owner_only_dir(&dir);
    let path = path_for(&send.temp_id);
    // Temp-then-rename so a crash mid-write can't leave a truncated entry that
    // fails the auth tag forever after.
    let tmp = path.with_extension("bin.tmp");
    if let Err(e) = std::fs::write(&tmp, &sealed) {
        eprintln!("[offline-queue] write {}: {e}", send.temp_id);
        return;
    }
    set_owner_only(&tmp);
    if let Err(e) = std::fs::rename(&tmp, &path) {
        eprintln!("[offline-queue] rename {}: {e}", send.temp_id);
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    set_owner_only(&path);
}

/// Drop a queued entry once its send is confirmed (acked). Best-effort.
pub fn remove(temp_id: &str) {
    let _ = std::fs::remove_file(path_for(temp_id));
}

/// Load every durable entry, sorted oldest-first (so a restart replays sends in
/// composition order). Entries that fail to open (corrupt, or sealed under a
/// previous vault password) are evicted. Does IO + crypto, so call it off the UI
/// thread.
pub fn load_all(vault: &Arc<Mutex<Vault>>) -> Vec<QueuedSend> {
    let Ok(entries) = std::fs::read_dir(queue_dir()) else {
        return Vec::new();
    };
    let mut out: Vec<QueuedSend> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("bin") {
            continue;
        }
        let Ok(sealed) = std::fs::read(&path) else {
            continue;
        };
        let plain = {
            let Ok(v) = vault.lock() else { return out };
            match v.open_blob(&sealed) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("[offline-queue] open {path:?}: {e}; evicting");
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
            }
        };
        match serde_json::from_slice::<QueuedSend>(&plain) {
            Ok(s) => out.push(s),
            Err(e) => {
                eprintln!("[offline-queue] decode {path:?}: {e}; evicting");
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    out.sort_by_key(|s| s.enqueued_at);
    out
}

/// Load a single durable entry by its temp id, or `None` if absent/unreadable.
/// Used by the manual retry path to recover an attachment's bytes (which the
/// in-RAM overlay does not keep) so a failed media send can be re-dispatched.
pub fn load_one(vault: &Arc<Mutex<Vault>>, temp_id: &str) -> Option<QueuedSend> {
    let sealed = std::fs::read(path_for(temp_id)).ok()?;
    let plain = {
        let v = vault.lock().ok()?;
        v.open_blob(&sealed).ok()?
    };
    serde_json::from_slice::<QueuedSend>(&plain).ok()
}

/// Delete the entire queue. Called on vault reset — entries sealed under the old
/// key are unreadable afterwards anyway.
pub fn clear() {
    let _ = std::fs::remove_dir_all(queue_dir());
}

#[cfg(unix)]
fn set_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) {}

#[cfg(unix)]
fn set_owner_only_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn set_owner_only_dir(_path: &Path) {}
