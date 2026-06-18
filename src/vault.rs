// Password-encrypted secret database. Replaces the old libsecret/pass storage.
//
// On disk this is a single file, `$DM_HOME/vault.db`, holding *every* secret the
// app keeps: the user's nsec plus marmot's per-account secret keys. Nothing is
// stored in plaintext and no OS keyring is touched.
//
// Layout — a serde envelope wrapping an AEAD-sealed key→value map:
//
//   VaultEnvelope { version, kdf{ argon2id salt + cost params }, nonce, ciphertext }
//      └─ ciphertext = XChaCha20-Poly1305( serde_json(BTreeMap<String,String>) )
//                      keyed by Argon2id(password, salt)
//
// `open` derives the key from the supplied password and AEAD-decrypts; a wrong
// password fails the Poly1305 tag check and surfaces as `VaultError::WrongPassword`.
// Every mutation re-seals the whole map under a fresh random nonce. The derived
// key is held in a `Zeroizing` buffer so it is wiped from memory on drop.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{Key, KeyInit, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use marmot_account::{AccountHomeError, AccountHomeResult, AccountSecretStore, AccountSummary};

/// Map key under which the *primary* account's nsec lives — the key the app
/// boots the runtime with. Kept for compatibility with single-account vaults;
/// additional accounts get [`nsec_key_for`] entries.
pub const NSEC_KEY: &str = "nsec";

/// Map key holding the account-id hex of the account the user last had
/// active, so a restart lands back on it. Not a secret, but it lives here so
/// the set of accounts isn't readable from plaintext config.
pub const ACTIVE_ACCOUNT_KEY: &str = "active_account";

/// Per-account nsec backup key. The runtime only needs the marmot secret
/// (`account:<label>`), but keeping the bech32 nsec per account means the
/// boot/self-heal path and a future "export key" affordance never have to
/// re-derive it.
pub fn nsec_key_for(account_id_hex: &str) -> String {
    format!("nsec:{}", account_id_hex.to_ascii_lowercase())
}

const VAULT_VERSION: u32 = 1;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24; // XChaCha20-Poly1305 uses a 192-bit nonce.

/// Domain-separation label mixed into the media-cache subkey derivation so it
/// can never coincide with the vault's own data-sealing key.
const MEDIA_CACHE_KDF_LABEL: &[u8] = b"darkmatter-linux/media-cache/v1";

// Argon2id cost parameters. ~19 MiB / 2 passes / 1 lane — the OWASP baseline.
// Stored in the envelope so a future tuning doesn't lock anyone out.
const ARGON_M_COST: u32 = 19_456;
const ARGON_T_COST: u32 = 2;
const ARGON_P_COST: u32 = 1;

#[derive(Debug)]
pub enum VaultError {
    /// Decryption failed the auth tag — almost always a wrong password.
    WrongPassword,
    /// File missing when one was expected.
    NotFound,
    /// Malformed envelope, bad hex, or unsupported version.
    Corrupt(String),
    Io(String),
    Crypto(String),
}

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VaultError::WrongPassword => write!(f, "wrong password"),
            VaultError::NotFound => write!(f, "vault not found"),
            VaultError::Corrupt(s) => write!(f, "vault corrupt: {s}"),
            VaultError::Io(s) => write!(f, "vault io: {s}"),
            VaultError::Crypto(s) => write!(f, "vault crypto: {s}"),
        }
    }
}

impl std::error::Error for VaultError {}

#[derive(Serialize, Deserialize)]
struct KdfParams {
    algo: String, // "argon2id"
    salt_hex: String,
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
}

#[derive(Serialize, Deserialize)]
struct VaultEnvelope {
    version: u32,
    kdf: KdfParams,
    nonce_hex: String,
    ciphertext_hex: String,
}

/// Default vault location: `$DM_HOME/vault.db`.
pub fn vault_path() -> PathBuf {
    crate::backend::default_home().join("vault.db")
}

/// The directory that holds `vault.db` (the data dir). Surfaced so the UI can
/// offer an "open vault folder" affordance — a user keeping their own backup of
/// the encrypted vault needs to know where it lives.
pub fn vault_dir() -> PathBuf {
    crate::backend::default_home()
}

/// Decrypt an in-memory vault envelope with `password` and extract every distinct
/// secret key it holds, as canonical bech32 nsecs. Used when merging a backup:
/// the `vault.db` inside it is re-logged account-by-account into the running app
/// (each returned nsec registers with marmot and re-seals into the *active*
/// vault). Merging raw maps wouldn't work — marmot's account list comes from its
/// own DB, not the secret store, so an account only becomes real once logged in.
///
/// Sources, all deduped by public key: the primary `nsec`, per-account
/// `nsec:<hex>` backups, and `account:<label>` marmot secret-key hex (the last
/// covers vaults that predate the per-account nsec backfill). A wrong password
/// surfaces as [`VaultError::WrongPassword`], same as a normal unlock.
pub fn import_nsecs_from_bytes(bytes: &[u8], password: &str) -> Result<Vec<String>, VaultError> {
    let plaintext = open_with_password(bytes, password)?;
    let data: BTreeMap<String, String> = serde_json::from_slice(&plaintext)
        .map_err(|e| VaultError::Corrupt(format!("inner json: {e}")))?;
    Ok(nsecs_from_map(&data))
}

/// Pull every distinct secret key out of a decrypted vault map, as bech32 nsecs,
/// deduped by public key. Sources: the primary `nsec`, `nsec:<hex>` backups, and
/// `account:<label>` marmot secret-key hex.
fn nsecs_from_map(data: &BTreeMap<String, String>) -> Vec<String> {
    use nostr::ToBech32;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<String> = Vec::new();
    for (k, val) in data {
        let is_secret = k == NSEC_KEY || k.starts_with("nsec:") || k.starts_with("account:");
        if !is_secret {
            continue;
        }
        // `Keys::parse` accepts both bech32 nsec (primary / nsec:* backups) and
        // raw secret-key hex (account:* marmot secrets), so one path covers all.
        let Ok(keys) = nostr::Keys::parse(val) else {
            continue;
        };
        if seen.insert(keys.public_key().to_hex()) {
            // `to_bech32` on a secret key is infallible here; `.ok()` keeps the
            // collect tidy without an irrefutable-pattern lint.
            out.extend(keys.secret_key().to_bech32().ok());
        }
    }
    out
}

/// Whether a vault file already exists (drives unlock-vs-create UI flow).
pub fn exists() -> bool {
    vault_path().exists()
}

/// Delete the vault file. Used by the "reset & use another key" escape on the
/// unlock screen — there is no password recovery, so a forgotten password means
/// starting over from the nsec.
pub fn delete() -> Result<(), VaultError> {
    // Cached media is sealed under a subkey of the (about-to-be-discarded) vault
    // key, so it would be undecryptable after a reset anyway — drop it too.
    crate::media_cache::clear();
    match std::fs::remove_file(vault_path()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(VaultError::Io(e.to_string())),
    }
}

/// An unlocked vault: the derived key plus the decrypted secret map, both held
/// in memory for the session. Mutations re-seal and persist immediately.
pub struct Vault {
    path: PathBuf,
    key: Zeroizing<[u8; 32]>,
    salt: [u8; SALT_LEN],
    data: BTreeMap<String, String>,
}

impl Vault {
    /// Create a fresh, empty vault sealed with `password`. Fails if one already
    /// exists (caller chooses unlock vs create).
    pub fn create(password: &str) -> Result<Self, VaultError> {
        let path = vault_path();
        if path.exists() {
            return Err(VaultError::Io("vault already exists".into()));
        }
        let mut salt = [0u8; SALT_LEN];
        random_bytes(&mut salt)?;
        let key = derive_key(password, &salt)?;
        let v = Vault {
            path,
            key,
            salt,
            data: BTreeMap::new(),
        };
        v.persist()?;
        Ok(v)
    }

    /// Open and decrypt the default vault (`$DM_HOME/vault.db`) with `password`.
    pub fn open(password: &str) -> Result<Self, VaultError> {
        Self::open_path(&vault_path(), password)
    }

    /// Open and decrypt the vault file at `path` with `password`. Backs both the
    /// default unlock ([`open`]) and reading a foreign vault for import.
    fn open_path(path: &Path, password: &str) -> Result<Self, VaultError> {
        let path = path.to_path_buf();
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(VaultError::NotFound),
            Err(e) => return Err(VaultError::Io(e.to_string())),
        };
        let (salt, plaintext, key) = open_envelope_keyed(&bytes, password)?;
        let data: BTreeMap<String, String> = serde_json::from_slice(&plaintext)
            .map_err(|e| VaultError::Corrupt(format!("inner json: {e}")))?;

        Ok(Vault {
            path,
            key,
            salt,
            data,
        })
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.data.get(key).map(|s| s.as_str())
    }

    pub fn has(&self, key: &str) -> bool {
        self.data.contains_key(key)
    }

    /// Insert/overwrite a secret and re-seal the file.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), VaultError> {
        self.data.insert(key.to_string(), value.to_string());
        self.persist()
    }

    /// Remove a secret and re-seal the file.
    pub fn remove(&mut self, key: &str) -> Result<(), VaultError> {
        if self.data.remove(key).is_some() {
            self.persist()?;
        }
        Ok(())
    }

    pub fn nsec(&self) -> Option<String> {
        self.get(NSEC_KEY).map(|s| s.to_string())
    }

    /// Subkey used to seal cached media blobs (see `media_cache.rs`). Derived
    /// from the vault's data key but domain-separated so the two uses can never
    /// interfere. The vault key is already a high-entropy 32-byte key, so a
    /// single SHA-256 over (label || key) is a sound KDF here — no HKDF needed.
    fn media_cache_key(&self) -> Zeroizing<[u8; 32]> {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(MEDIA_CACHE_KDF_LABEL);
        h.update(*self.key);
        let mut k = Zeroizing::new([0u8; 32]);
        k.copy_from_slice(&h.finalize());
        k
    }

    /// Seal an arbitrary blob (e.g. a decrypted attachment) for at-rest storage.
    /// Layout is `nonce(24) || XChaCha20-Poly1305(plaintext)`, keyed by the
    /// media-cache subkey. Used by the encrypted media cache so decrypted
    /// attachments never touch the disk in the clear.
    pub fn seal_blob(&self, plaintext: &[u8]) -> Result<Vec<u8>, VaultError> {
        let key = self.media_cache_key();
        let mut nonce = [0u8; NONCE_LEN];
        random_bytes(&mut nonce)?;
        let cipher = XChaCha20Poly1305::new(Key::from_slice(&*key));
        let ciphertext = cipher
            .encrypt(XNonce::from_slice(&nonce), plaintext)
            .map_err(|e| VaultError::Crypto(e.to_string()))?;
        let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Reverse of [`seal_blob`]. Returns the plaintext, or an error if the blob
    /// is truncated or fails the auth tag (corruption, or sealed under a key
    /// from a previous vault password).
    pub fn open_blob(&self, sealed: &[u8]) -> Result<Vec<u8>, VaultError> {
        if sealed.len() < NONCE_LEN {
            return Err(VaultError::Corrupt("sealed blob too short".into()));
        }
        let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
        let key = self.media_cache_key();
        let cipher = XChaCha20Poly1305::new(Key::from_slice(&*key));
        cipher
            .decrypt(XNonce::from_slice(nonce), ciphertext)
            .map_err(|_| VaultError::WrongPassword)
    }

    /// Encrypt the current map under a fresh nonce and atomically write the file.
    fn persist(&self) -> Result<(), VaultError> {
        let plaintext =
            serde_json::to_vec(&self.data).map_err(|e| VaultError::Crypto(e.to_string()))?;
        let mut nonce = [0u8; NONCE_LEN];
        random_bytes(&mut nonce)?;
        let cipher = XChaCha20Poly1305::new(Key::from_slice(&*self.key));
        let ciphertext = cipher
            .encrypt(XNonce::from_slice(&nonce), plaintext.as_ref())
            .map_err(|e| VaultError::Crypto(e.to_string()))?;

        let env = VaultEnvelope {
            version: VAULT_VERSION,
            kdf: KdfParams {
                algo: "argon2id".to_string(),
                salt_hex: hex::encode(self.salt),
                m_cost: ARGON_M_COST,
                t_cost: ARGON_T_COST,
                p_cost: ARGON_P_COST,
            },
            nonce_hex: hex::encode(nonce),
            ciphertext_hex: hex::encode(&ciphertext),
        };
        let bytes = serde_json::to_vec(&env).map_err(|e| VaultError::Crypto(e.to_string()))?;

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| VaultError::Io(e.to_string()))?;
        }
        // Write to a temp sibling then rename, so a crash mid-write can't truncate
        // the existing vault.
        let tmp = self.path.with_extension("db.tmp");
        std::fs::write(&tmp, &bytes).map_err(|e| VaultError::Io(e.to_string()))?;
        set_owner_only(&tmp);
        std::fs::rename(&tmp, &self.path).map_err(|e| VaultError::Io(e.to_string()))?;
        set_owner_only(&self.path);
        Ok(())
    }
}

fn derive_key(password: &str, salt: &[u8; SALT_LEN]) -> Result<Zeroizing<[u8; 32]>, VaultError> {
    derive_key_with_params(password, salt, ARGON_M_COST, ARGON_T_COST, ARGON_P_COST)
}

/// Reject Argon2 cost parameters read from an untrusted envelope before they
/// reach the deriver. The ceilings are far above what we ever seal with
/// (`ARGON_*_COST`) yet bound the worst-case allocation/CPU so a crafted or
/// corrupt file can't turn unlock into a denial of service. `m_cost` is the
/// dominant one — it's the Argon2 memory in KiB, so the cap is ~1 GiB.
fn check_kdf_cost(m_cost: u32, t_cost: u32, p_cost: u32) -> Result<(), VaultError> {
    const MAX_M_COST: u32 = 1 << 20; // 1 GiB of Argon2 memory.
    const MAX_T_COST: u32 = 64;
    const MAX_P_COST: u32 = 64;
    if m_cost > MAX_M_COST || t_cost > MAX_T_COST || p_cost > MAX_P_COST {
        return Err(VaultError::Corrupt(format!(
            "kdf cost out of range (m={m_cost} t={t_cost} p={p_cost})"
        )));
    }
    Ok(())
}

fn derive_key_with_params(
    password: &str,
    salt: &[u8],
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
) -> Result<Zeroizing<[u8; 32]>, VaultError> {
    let params = Params::new(m_cost, t_cost, p_cost, Some(32))
        .map_err(|e| VaultError::Crypto(format!("argon params: {e}")))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0u8; 32]);
    argon
        .hash_password_into(password.as_bytes(), salt, &mut *key)
        .map_err(|e| VaultError::Crypto(format!("argon derive: {e}")))?;
    Ok(key)
}

fn random_bytes(buf: &mut [u8]) -> Result<(), VaultError> {
    getrandom::getrandom(buf).map_err(|e| VaultError::Crypto(format!("rng: {e}")))
}

/// `(salt, recovered plaintext, derived Argon2id key)` — the parts of an opened
/// envelope.
type OpenedEnvelope = ([u8; SALT_LEN], Vec<u8>, Zeroizing<[u8; 32]>);

/// Parse and decrypt a `VaultEnvelope` JSON blob, returning the salt, the
/// recovered plaintext, and the derived Argon2id key. The single decrypt path
/// behind [`Vault::open_path`] and the generic [`open_with_password`].
fn open_envelope_keyed(bytes: &[u8], password: &str) -> Result<OpenedEnvelope, VaultError> {
    let env: VaultEnvelope =
        serde_json::from_slice(bytes).map_err(|e| VaultError::Corrupt(e.to_string()))?;
    if env.version != VAULT_VERSION {
        return Err(VaultError::Corrupt(format!(
            "unsupported vault version {}",
            env.version
        )));
    }
    if env.kdf.algo != "argon2id" {
        return Err(VaultError::Corrupt(format!("unknown kdf {}", env.kdf.algo)));
    }
    let salt_vec = hex::decode(&env.kdf.salt_hex)
        .map_err(|e| VaultError::Corrupt(format!("salt hex: {e}")))?;
    let salt: [u8; SALT_LEN] = salt_vec
        .as_slice()
        .try_into()
        .map_err(|_| VaultError::Corrupt("bad salt length".into()))?;
    let nonce_vec =
        hex::decode(&env.nonce_hex).map_err(|e| VaultError::Corrupt(format!("nonce hex: {e}")))?;
    // A malformed envelope must not panic `XNonce::from_slice` (which requires
    // exactly NONCE_LEN bytes) — turn a bad length into a clean Corrupt error,
    // mirroring the salt check above.
    let nonce: [u8; NONCE_LEN] = nonce_vec
        .as_slice()
        .try_into()
        .map_err(|_| VaultError::Corrupt("bad nonce length".into()))?;
    let ciphertext = hex::decode(&env.ciphertext_hex)
        .map_err(|e| VaultError::Corrupt(format!("ciphertext hex: {e}")))?;

    // The KDF parameters ride in the (unauthenticated) envelope, so a hostile or
    // corrupt file could ask for an absurd Argon2 cost to wedge unlock/import in
    // a multi-gigabyte allocation. Reject anything past a generous ceiling before
    // deriving — legitimate vaults are sealed at ARGON_*_COST.
    check_kdf_cost(env.kdf.m_cost, env.kdf.t_cost, env.kdf.p_cost)?;
    let key = derive_key_with_params(
        password,
        &salt,
        env.kdf.m_cost,
        env.kdf.t_cost,
        env.kdf.p_cost,
    )?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&*key));
    let plaintext = cipher
        .decrypt(XNonce::from_slice(&nonce), ciphertext.as_ref())
        // The only realistic decrypt failure here is a bad auth tag => wrong password.
        .map_err(|_| VaultError::WrongPassword)?;
    Ok((salt, plaintext, key))
}

/// Seal arbitrary bytes into a fresh `VaultEnvelope` JSON blob keyed by
/// `Argon2id(password)`. Shares the vault's on-disk format and cost parameters
/// so the folder-backup file (`backup.rs`) is the same crypto as the vault, just
/// wrapping a tar instead of the secret map.
pub(crate) fn seal_with_password(password: &str, plaintext: &[u8]) -> Result<Vec<u8>, VaultError> {
    let mut salt = [0u8; SALT_LEN];
    random_bytes(&mut salt)?;
    let key = derive_key(password, &salt)?;
    let mut nonce = [0u8; NONCE_LEN];
    random_bytes(&mut nonce)?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&*key));
    let ciphertext = cipher
        .encrypt(XNonce::from_slice(&nonce), plaintext)
        .map_err(|e| VaultError::Crypto(e.to_string()))?;
    let env = VaultEnvelope {
        version: VAULT_VERSION,
        kdf: KdfParams {
            algo: "argon2id".to_string(),
            salt_hex: hex::encode(salt),
            m_cost: ARGON_M_COST,
            t_cost: ARGON_T_COST,
            p_cost: ARGON_P_COST,
        },
        nonce_hex: hex::encode(nonce),
        ciphertext_hex: hex::encode(&ciphertext),
    };
    serde_json::to_vec(&env).map_err(|e| VaultError::Crypto(e.to_string()))
}

/// Reverse of [`seal_with_password`]: decrypt a `VaultEnvelope` blob to its raw
/// plaintext. Wrong password surfaces as [`VaultError::WrongPassword`].
pub(crate) fn open_with_password(bytes: &[u8], password: &str) -> Result<Vec<u8>, VaultError> {
    let (_salt, plaintext, _key) = open_envelope_keyed(bytes, password)?;
    Ok(plaintext)
}

#[cfg(unix)]
fn set_owner_only(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_owner_only(_path: &std::path::Path) {}

// ── marmot AccountSecretStore backed by the vault ────────────────────────
//
// marmot's AccountHome stores each account's secret key through this trait.
// Routing it here means the account secret lands in the same encrypted file as
// the nsec instead of libsecret (KeychainSecretStore) or plaintext JSON
// (LocalFileSecretStore). Per-account secrets live under `account:<label>`.

fn account_key(label: &str) -> String {
    format!("account:{label}")
}

pub struct VaultSecretStore {
    vault: Arc<Mutex<Vault>>,
}

impl VaultSecretStore {
    pub fn new(vault: Arc<Mutex<Vault>>) -> Self {
        Self { vault }
    }
}

impl AccountSecretStore for VaultSecretStore {
    fn has_secret_for_label(&self, label: &str) -> AccountHomeResult<bool> {
        let v = self.vault.lock().unwrap();
        Ok(v.has(&account_key(label)))
    }

    fn write_secret(&self, account: &AccountSummary, keys: &nostr::Keys) -> AccountHomeResult<()> {
        let mut v = self.vault.lock().unwrap();
        v.set(
            &account_key(&account.label),
            &keys.secret_key().to_secret_hex(),
        )
        .map_err(|e| AccountHomeError::SecretStore(e.to_string()))
    }

    fn load_secret(&self, account: &AccountSummary) -> AccountHomeResult<nostr::Keys> {
        let v = self.vault.lock().unwrap();
        let hex = v
            .get(&account_key(&account.label))
            .ok_or_else(|| AccountHomeError::SecretNotFound(account.label.clone()))?;
        nostr::Keys::parse(hex).map_err(|_| AccountHomeError::InvalidSecretKey)
    }

    fn remove_secret(&self, account: &AccountSummary) -> AccountHomeResult<()> {
        let mut v = self.vault.lock().unwrap();
        v.remove(&account_key(&account.label))
            .map_err(|e| AccountHomeError::SecretStore(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Points the vault at a unique temp dir via DM_HOME, runs `f`, then cleans up.
    // Holds the shared DM_HOME lock so the backup suite (which also rebinds the
    // env var) can't run concurrently.
    fn with_temp_home(f: impl FnOnce()) {
        let _guard = crate::DM_HOME_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("DM_HOME");
        let dir = std::env::temp_dir().join(format!("dm-vault-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: the lock above guarantees no other test reads/writes DM_HOME
        // concurrently.
        unsafe {
            std::env::set_var("DM_HOME", &dir);
        }
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        // Restore DM_HOME (even on panic) so a later test doesn't inherit a path
        // that points at the temp dir we're about to delete.
        unsafe {
            match &prev {
                Some(v) => std::env::set_var("DM_HOME", v),
                None => std::env::remove_var("DM_HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        if let Err(e) = r {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    fn roundtrip_create_open_and_wrong_password() {
        with_temp_home(|| {
            assert!(!exists());
            {
                let mut v = Vault::create("correct horse battery").unwrap();
                v.set(NSEC_KEY, "nsec1example").unwrap();
                v.set(&account_key("alice"), "deadbeef").unwrap();
            }
            assert!(exists());

            // Right password: secrets survive a reload.
            let v = Vault::open("correct horse battery").unwrap();
            assert_eq!(v.nsec().as_deref(), Some("nsec1example"));
            assert_eq!(v.get(&account_key("alice")), Some("deadbeef"));

            // Wrong password: auth tag fails, surfaced as WrongPassword.
            match Vault::open("wrong password") {
                Err(VaultError::WrongPassword) => {}
                Err(other) => panic!("expected WrongPassword, got {other:?}"),
                Ok(_) => panic!("expected WrongPassword, got Ok"),
            }

            // Creating over an existing vault is refused.
            assert!(Vault::create("whatever").is_err());

            // ── Media-cache blob sealing (seal_blob / open_blob) ──────────
            let v = Vault::open("correct horse battery").unwrap();
            let plaintext = b"\x89PNG\r\n\x1a\n decrypted attachment bytes".to_vec();
            let sealed = v.seal_blob(&plaintext).unwrap();
            // Overhead = nonce + auth tag; plaintext doesn't leak verbatim.
            assert!(sealed.len() > plaintext.len());
            assert_eq!(v.open_blob(&sealed).unwrap(), plaintext);

            // A truncated blob is rejected, not panicked on.
            assert!(matches!(
                v.open_blob(&sealed[..NONCE_LEN - 1]),
                Err(VaultError::Corrupt(_))
            ));

            // A blob sealed under one vault key can't be opened by another.
            // (Built directly — the test module can see private fields — so we
            // don't have to juggle DM_HOME for a second on-disk vault.)
            let mut foreign_key = v.key.clone();
            foreign_key[0] ^= 0xff;
            let foreign = Vault {
                path: v.path.clone(),
                key: foreign_key,
                salt: v.salt,
                data: BTreeMap::new(),
            };
            assert!(matches!(
                foreign.open_blob(&sealed),
                Err(VaultError::WrongPassword)
            ));

            // ── import_nsecs: pull every distinct key out of a foreign vault ──
            use nostr::ToBech32;
            let k1 = nostr::Keys::generate();
            let k2 = nostr::Keys::generate();
            let k3 = nostr::Keys::generate();
            let import_path = vault_path().with_file_name("import-test.db");
            {
                // Build a foreign vault directly (the test module sees private
                // fields) holding all three key encodings the importer handles:
                // primary bech32 nsec, an nsec:<hex> backup, and an account:<label>
                // raw secret-hex — plus a duplicate of k1 that must dedup away.
                let mut salt = [0u8; SALT_LEN];
                random_bytes(&mut salt).unwrap();
                let key = derive_key("pw2", &salt).unwrap();
                let mut data = BTreeMap::new();
                data.insert(NSEC_KEY.to_string(), k1.secret_key().to_bech32().unwrap());
                data.insert(
                    nsec_key_for(&k2.public_key().to_hex()),
                    k2.secret_key().to_bech32().unwrap(),
                );
                data.insert(account_key("work"), k3.secret_key().to_secret_hex());
                data.insert(account_key("dup-of-k1"), k1.secret_key().to_secret_hex());
                let fv = Vault {
                    path: import_path.clone(),
                    key,
                    salt,
                    data,
                };
                fv.persist().unwrap();
            }

            let bytes = std::fs::read(&import_path).unwrap();
            let got = import_nsecs_from_bytes(&bytes, "pw2").unwrap();
            let have: BTreeSet<String> = got.into_iter().collect();
            let want: BTreeSet<String> = [&k1, &k2, &k3]
                .iter()
                .map(|k| k.secret_key().to_bech32().unwrap())
                .collect();
            assert_eq!(have, want, "all three distinct keys, k1 deduped");

            // Wrong password for the foreign vault surfaces as WrongPassword.
            assert!(matches!(
                import_nsecs_from_bytes(&bytes, "nope"),
                Err(VaultError::WrongPassword)
            ));

            // Generic seal/open round-trips arbitrary bytes (backs backup.rs).
            let blob = b"darkmatter folder archive bytes".to_vec();
            let sealed = seal_with_password("pw2", &blob).unwrap();
            assert_eq!(open_with_password(&sealed, "pw2").unwrap(), blob);
            assert!(matches!(
                open_with_password(&sealed, "wrong"),
                Err(VaultError::WrongPassword)
            ));
        });
    }
}
