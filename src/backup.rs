// Whole-folder encrypted backup & restore.
//
// A backup is the entire `$DM_HOME` data directory — the vault, marmot's
// account/group databases, key packages, the offline queue — packed into a
// single file and sealed with the user's *vault* password. Using the same
// password the vault already uses means a restore needs exactly one secret: the
// extracted vault unlocks with the very password that decrypted the archive.
//
// Layout: the plaintext is a trivial length-prefixed archive (no external tar
// dependency); the whole thing is then sealed in the same `VaultEnvelope`
// (Argon2id + XChaCha20-Poly1305) the vault uses — see `vault::seal_with_password`.
//
//   archive := MAGIC(6) ‖ u32 count ‖ count × ( u16 path_len ‖ path ‖ u64 len ‖ bytes )
//
// The encrypted media cache is deliberately excluded: it's a regenerable cache
// (re-downloaded + re-decrypted on demand) and would bloat the backup with the
// largest bytes in the directory. `*.tmp` scratch files are skipped too.

use std::path::{Path, PathBuf};

use crate::vault;

const MAGIC: &[u8; 6] = b"DMBK01";
/// Suggested filename for the save dialog. The user can rename freely.
pub const DEFAULT_FILENAME: &str = "darkmatter-backup.dmbackup";

#[derive(Debug)]
pub enum BackupError {
    /// The password didn't decrypt the archive (or the vault, at backup time).
    WrongPassword,
    /// No backup/vault file where one was expected.
    NotFound,
    /// Malformed archive or envelope.
    Corrupt(String),
    Io(String),
    Crypto(String),
}

impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackupError::WrongPassword => write!(f, "wrong password"),
            BackupError::NotFound => write!(f, "backup not found"),
            BackupError::Corrupt(s) => write!(f, "backup corrupt: {s}"),
            BackupError::Io(s) => write!(f, "backup io: {s}"),
            BackupError::Crypto(s) => write!(f, "backup crypto: {s}"),
        }
    }
}

impl std::error::Error for BackupError {}

impl From<vault::VaultError> for BackupError {
    fn from(e: vault::VaultError) -> Self {
        match e {
            vault::VaultError::WrongPassword => BackupError::WrongPassword,
            vault::VaultError::NotFound => BackupError::NotFound,
            vault::VaultError::Corrupt(s) => BackupError::Corrupt(s),
            vault::VaultError::Io(s) => BackupError::Io(s),
            vault::VaultError::Crypto(s) => BackupError::Crypto(s),
        }
    }
}

/// Create an encrypted backup of the whole data dir at `dest`, sealed with the
/// vault `password`. The password is verified against the current vault first,
/// so a typo fails fast (and an empty/locked install can't produce a backup
/// nobody can open).
pub fn create(dest: &Path, password: &str) -> Result<(), BackupError> {
    // Authorize + validate: only the real vault password may seal a backup.
    vault::Vault::open(password)?;

    let home = vault::vault_dir();
    let dest_canon = dest.canonicalize().ok();
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    collect_files(&home, &home, &dest_canon, &mut entries)?;

    let archive = pack(&entries);
    let sealed = vault::seal_with_password(password, &archive)?;

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| BackupError::Io(e.to_string()))?;
    }
    let tmp = dest.with_extension("dmbackup.tmp");
    std::fs::write(&tmp, &sealed).map_err(|e| BackupError::Io(e.to_string()))?;
    set_owner_only(&tmp);
    std::fs::rename(&tmp, dest).map_err(|e| BackupError::Io(e.to_string()))?;
    set_owner_only(dest);
    Ok(())
}

/// Decrypt the backup at `src` with `password` and extract it into the data dir,
/// overwriting files that collide. Intended for a fresh install (no vault yet) —
/// callers gate on [`vault::exists`] so an existing identity is never clobbered.
pub fn restore_into_home(src: &Path, password: &str) -> Result<(), BackupError> {
    let entries = read_archive(src, password)?;
    let home = vault::vault_dir();
    std::fs::create_dir_all(&home).map_err(|e| BackupError::Io(e.to_string()))?;
    for (rel, bytes) in &entries {
        let dest = safe_join(&home, rel)?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| BackupError::Io(e.to_string()))?;
        }
        std::fs::write(&dest, bytes).map_err(|e| BackupError::Io(e.to_string()))?;
        set_owner_only(&dest);
    }
    Ok(())
}

/// Decrypt the backup at `src` and pull every secret key out of the `vault.db`
/// it carries (as bech32 nsecs). Used to *merge* a backup into an already
/// running install: marmot's account list is DB-backed, so the only safe merge
/// is to re-login each key rather than overlay live databases.
pub fn merge_nsecs(src: &Path, password: &str) -> Result<Vec<String>, BackupError> {
    let entries = read_archive(src, password)?;
    let vault_db = entries
        .iter()
        .find(|(rel, _)| rel == "vault.db")
        .map(|(_, bytes)| bytes)
        .ok_or_else(|| BackupError::Corrupt("backup has no vault.db".into()))?;
    // The embedded vault.db is itself sealed with the same password (== the
    // backup password), so this second decrypt just works.
    Ok(vault::import_nsecs_from_bytes(vault_db, password)?)
}

// ── internals ────────────────────────────────────────────────────────────

/// Read + decrypt + unpack a backup file into its (relative-path, bytes) list.
fn read_archive(src: &Path, password: &str) -> Result<Vec<(String, Vec<u8>)>, BackupError> {
    let sealed = match std::fs::read(src) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(BackupError::NotFound),
        Err(e) => return Err(BackupError::Io(e.to_string())),
    };
    let archive = vault::open_with_password(&sealed, password)?;
    unpack(&archive)
}

/// Recursively gather files under `dir` as (relative-to-`root`, bytes), skipping
/// the regenerable media cache, `*.tmp` scratch, and the backup file itself.
fn collect_files(
    root: &Path,
    dir: &Path,
    dest_canon: &Option<PathBuf>,
    out: &mut Vec<(String, Vec<u8>)>,
) -> Result<(), BackupError> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(BackupError::Io(e.to_string())),
    };
    for entry in rd {
        let entry = entry.map_err(|e| BackupError::Io(e.to_string()))?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Skip the encrypted media cache wholesale — it's regenerable and the
        // bulkiest thing in the directory.
        if path.is_dir() {
            if name == "media-cache" {
                continue;
            }
            collect_files(root, &path, dest_canon, out)?;
            continue;
        }
        if name.ends_with(".tmp") {
            continue;
        }
        // Don't fold the backup file into itself if it's being written inside home.
        if let (Ok(canon), Some(dest)) = (path.canonicalize(), dest_canon)
            && canon == *dest
        {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .map_err(|e| BackupError::Io(e.to_string()))?;
        let rel = rel_to_slash(rel);
        let bytes = std::fs::read(&path).map_err(|e| BackupError::Io(e.to_string()))?;
        out.push((rel, bytes));
    }
    Ok(())
}

/// Render a relative path with `/` separators, regardless of platform.
fn rel_to_slash(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Join a slash-separated archive path onto `root`, rejecting anything that
/// would escape it (absolute, `..`, or empty components) — a malformed or
/// hostile archive must not write outside the data dir.
fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, BackupError> {
    let mut out = root.to_path_buf();
    for comp in rel.split('/') {
        if comp.is_empty() || comp == "." || comp == ".." || comp.contains('\\') {
            return Err(BackupError::Corrupt(format!("unsafe path in backup: {rel}")));
        }
        out.push(comp);
    }
    Ok(out)
}

fn pack(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (path, bytes) in entries {
        let p = path.as_bytes();
        out.extend_from_slice(&(p.len() as u16).to_le_bytes());
        out.extend_from_slice(p);
        out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        out.extend_from_slice(bytes);
    }
    out
}

fn unpack(buf: &[u8]) -> Result<Vec<(String, Vec<u8>)>, BackupError> {
    let mut cur = buf;
    let take = |cur: &mut &[u8], n: usize| -> Result<Vec<u8>, BackupError> {
        if cur.len() < n {
            return Err(BackupError::Corrupt("archive truncated".into()));
        }
        let (head, tail) = cur.split_at(n);
        *cur = tail;
        Ok(head.to_vec())
    };
    if take(&mut cur, MAGIC.len())? != MAGIC {
        return Err(BackupError::Corrupt("bad magic".into()));
    }
    let count = u32::from_le_bytes(
        take(&mut cur, 4)?
            .try_into()
            .map_err(|_| BackupError::Corrupt("count".into()))?,
    ) as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let plen = u16::from_le_bytes(
            take(&mut cur, 2)?
                .try_into()
                .map_err(|_| BackupError::Corrupt("path len".into()))?,
        ) as usize;
        let path = String::from_utf8(take(&mut cur, plen)?)
            .map_err(|_| BackupError::Corrupt("path utf8".into()))?;
        let dlen = u64::from_le_bytes(
            take(&mut cur, 8)?
                .try_into()
                .map_err(|_| BackupError::Corrupt("data len".into()))?,
        ) as usize;
        let data = take(&mut cur, dlen)?;
        out.push((path, data));
    }
    Ok(out)
}

#[cfg(unix)]
fn set_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_roundtrip_and_unsafe_paths() {
        let entries = vec![
            ("vault.db".to_string(), b"\x00\x01\x02sealed".to_vec()),
            ("key-packages/a.bin".to_string(), vec![0xffu8; 300]),
            ("empty".to_string(), Vec::new()),
        ];
        let packed = pack(&entries);
        assert_eq!(&packed[..MAGIC.len()], MAGIC);
        let got = unpack(&packed).unwrap();
        assert_eq!(got, entries);

        // Truncation is rejected, not panicked on.
        assert!(matches!(
            unpack(&packed[..packed.len() - 1]),
            Err(BackupError::Corrupt(_))
        ));
        assert!(matches!(unpack(b"nope"), Err(BackupError::Corrupt(_))));

        // Path-traversal entries can't escape the data dir.
        let root = Path::new("/home/u/.dm");
        assert!(safe_join(root, "key-packages/a.bin").is_ok());
        for bad in ["../escape", "/etc/passwd", "a/../../b", ""] {
            assert!(
                matches!(safe_join(root, bad), Err(BackupError::Corrupt(_))),
                "expected reject for {bad:?}"
            );
        }
    }

    #[test]
    fn create_restore_merge_roundtrip() {
        use nostr::ToBech32;
        // Shared with the vault suite — both rebind DM_HOME.
        let _guard = crate::DM_HOME_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let home = std::env::temp_dir().join(format!("dm-backup-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: the lock above serializes all DM_HOME access in tests.
        unsafe {
            std::env::set_var("DM_HOME", &home);
        }

        // A vault with a primary nsec + a second account key, plus a couple of
        // sibling files and a media-cache dir to exercise the excludes.
        let k1 = nostr::Keys::generate();
        let k2 = nostr::Keys::generate();
        {
            let mut v = vault::Vault::create("pw").unwrap();
            v.set(vault::NSEC_KEY, &k1.secret_key().to_bech32().unwrap())
                .unwrap();
            v.set("account:work", &k2.secret_key().to_secret_hex())
                .unwrap();
        }
        std::fs::create_dir_all(home.join("media-cache")).unwrap();
        std::fs::write(home.join("media-cache/big.bin"), vec![7u8; 64]).unwrap();
        std::fs::write(home.join("shared.sqlite3"), b"fake marmot db").unwrap();
        std::fs::write(home.join("scratch.tmp"), b"junk").unwrap();

        // Create the backup outside the home dir.
        let dest = std::env::temp_dir().join(format!("dm-backup-{}.dmbackup", std::process::id()));
        let _ = std::fs::remove_file(&dest);
        create(&dest, "pw").unwrap();
        assert!(matches!(create(&dest, "nope"), Err(BackupError::WrongPassword)));

        // merge_nsecs recovers both keys (deduped, bech32).
        let got: std::collections::BTreeSet<String> =
            merge_nsecs(&dest, "pw").unwrap().into_iter().collect();
        let want: std::collections::BTreeSet<String> = [&k1, &k2]
            .iter()
            .map(|k| k.secret_key().to_bech32().unwrap())
            .collect();
        assert_eq!(got, want);
        assert!(matches!(
            merge_nsecs(&dest, "nope"),
            Err(BackupError::WrongPassword)
        ));

        // Wipe the home, restore, and confirm what came back (and what didn't).
        std::fs::remove_dir_all(&home).unwrap();
        restore_into_home(&dest, "pw").unwrap();
        assert!(home.join("vault.db").exists());
        assert!(home.join("shared.sqlite3").exists());
        assert!(!home.join("media-cache/big.bin").exists(), "cache excluded");
        assert!(!home.join("scratch.tmp").exists(), "tmp excluded");
        // The restored vault unlocks with the same password and yields k1's nsec.
        let v = vault::Vault::open("pw").unwrap();
        assert_eq!(v.nsec().unwrap(), k1.secret_key().to_bech32().unwrap());

        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_file(&dest);
    }
}
