// Encrypted-at-rest disk cache for decrypted media attachments.
//
// Decrypted attachment bytes are sensitive, and this repo deliberately keeps no
// plaintext on disk (see `vault.rs`). So each entry is sealed with the vault's
// media-cache subkey (XChaCha20-Poly1305) before being written to
// `$DM_HOME/media-cache/<file_hash>.bin` (mode 0600). Entries are
// content-addressed by the Blossom blob hash (the `x` field of the NIP-92
// `imeta` tag), so the same attachment referenced from several messages shares
// one entry, and a download whose ciphertext hash mismatches can never collide
// with a good entry.
//
// The whole cache is best-effort: any IO or crypto failure degrades to a miss
// and the caller falls back to a fresh Blossom download + decrypt. We store the
// decrypted *original* bytes (the compressed PNG/JPEG/…), not decoded RGBA —
// far smaller on disk, and re-decoding locally is cheap next to the network
// round-trip and decryption we're avoiding.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::vault::Vault;

fn cache_dir() -> PathBuf {
    crate::backend::default_home().join("media-cache")
}

/// Whether `hash_hex` is a safe, hex-only filename component. Guards the path
/// join against traversal even though these values come from parsed imeta tags.
fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn path_for(hash_hex: &str) -> Option<PathBuf> {
    is_hex(hash_hex).then(|| cache_dir().join(format!("{hash_hex}.bin")))
}

/// Look up a previously cached attachment by its blob hash. Returns the
/// decrypted plaintext, or `None` on any miss (absent / unreadable / sealed
/// under a stale key). A stale entry is evicted so a fresh download can
/// repopulate it.
pub fn get(vault: &Arc<Mutex<Vault>>, hash_hex: &str) -> Option<Vec<u8>> {
    let path = path_for(hash_hex)?;
    let sealed = std::fs::read(&path).ok()?;
    let v = vault.lock().ok()?;
    match v.open_blob(&sealed) {
        Ok(plain) => Some(plain),
        Err(e) => {
            eprintln!("[media-cache] open {hash_hex}: {e}; evicting");
            let _ = std::fs::remove_file(&path);
            None
        }
    }
}

/// Seal `plaintext` under the vault key and write it to the cache. Best-effort:
/// failures are logged and swallowed — the in-memory cache still holds the live
/// copy for the rest of this session.
pub fn put(vault: &Arc<Mutex<Vault>>, hash_hex: &str, plaintext: &[u8]) {
    let Some(path) = path_for(hash_hex) else {
        return;
    };
    let sealed = {
        let Ok(v) = vault.lock() else { return };
        match v.seal_blob(plaintext) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[media-cache] seal {hash_hex}: {e}");
                return;
            }
        }
    };
    let dir = cache_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("[media-cache] mkdir: {e}");
        return;
    }
    set_owner_only_dir(&dir);
    // Temp-then-rename so a crash mid-write can't leave a truncated entry that
    // would fail the auth tag (and waste a re-download) every time after.
    let tmp = path.with_extension("bin.tmp");
    if let Err(e) = std::fs::write(&tmp, &sealed) {
        eprintln!("[media-cache] write {hash_hex}: {e}");
        return;
    }
    set_owner_only(&tmp);
    if let Err(e) = std::fs::rename(&tmp, &path) {
        eprintln!("[media-cache] rename {hash_hex}: {e}");
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    set_owner_only(&path);
}

/// Delete the entire media cache. Called when the vault is reset, since entries
/// sealed under the old key are unreadable afterwards anyway.
pub fn clear() {
    let _ = std::fs::remove_dir_all(cache_dir());
}

/// Total size on disk of the sealed cache entries, in bytes. Walks the cache
/// dir (flat — no subdirs) and sums file lengths. Best-effort: an unreadable
/// dir or entry just contributes nothing. Does IO, so call it off the UI thread.
pub fn size_bytes() -> u64 {
    let Ok(entries) = std::fs::read_dir(cache_dir()) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
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
