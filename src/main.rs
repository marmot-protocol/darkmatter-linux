use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use marmot_app::{
    AppGroupMemberRecord, AppGroupRecord, AppMessageRecord, AuditLogFile, MediaAttachmentReference,
    MediaLocator, UserDirectoryRecord, UserProfileMetadata, npub_for_account_id,
};
use nostr::Keys;
use nostr::nips::nip19::ToBech32;
use slint::{Color, Model, ModelRc, SharedString, VecModel, Weak};
use tokio::task::JoinHandle;

mod animal_avatar;
mod audio;
mod backend;
mod backup;
mod blossom;
mod media_cache;
mod mpv;
mod notify;
mod observability;
mod offline_queue;
mod settings;
mod unread;
mod vault;

use backend::Backend;
use settings::Settings;
use vault::Vault;

// Tests that point `DM_HOME` at a temp dir mutate a single process-global env
// var, so the vault and backup suites must not run concurrently — they share
// this lock to serialize. (Poisoning is ignored: a panicking test still leaves
// the lock usable for the next.)
#[cfg(test)]
pub(crate) static DM_HOME_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// Generated Slint UI (components, ui/tokens.slint structs, globals) plus the
// build-time emoji sprite artifacts — all owned by the dm-ui crate so Rust
// edits here don't recompile the generated UI module.
use dm_ui::*;

// ─── Optimistic-rendering state ────────────────────────────────────────
//
// All UI mutations (send / react / unreact) used to be synchronous: the UI
// blocked on the marmot round-trip, then a refresh repainted from the
// backend snapshot. That model produced ~100–1500ms of frozen UI per send.
//
// The new model is:
//   1. UI applies the change locally to an "overlay" (this struct).
//   2. UI rebuilds the message row from `backend snapshot + overlay`.
//   3. The real send is dispatched on the tokio runtime in the background.
//   4. On ack: drop the overlay entry, rebuild (snapshot now has the real
//      record, so the row keeps the same content but the bubble flips from
//      pending → confirmed).
//   5. On failure: mark the overlay entry failed (red bubble, tap to retry).
//
// The overlay only ever holds *my* not-yet-confirmed mutations. Everything
// else still comes from the marmot snapshot.

#[derive(Clone)]
struct PendingSend {
    // Local-only id so retry/failure can find the entry. Carried into the
    // bubble's `message_id` so the retry callback can resolve back here.
    temp_id: String,
    text: String,
    failed: bool,
    // When `Some`, this send is a reply — sent via `reply_to_message` so the
    // event carries `e`+`q` tags. The triple is (parent_id_hex, author_label,
    // preview_text) — same values we render in the chip + quoted block.
    reply_to: Option<(String, String, String)>,
    // Media upload + send. Empty for a plain text send; one entry for a single
    // attachment (chip/image preview); 2+ for an album (rendered as a grid).
    // The optimistic bubble renders straight from the local previews while the
    // encrypt+blossom+publish round-trip resolves.
    media: Vec<PendingMedia>,
    // Armed message effect (Telegram-style burst), 0 = none. Plays once on the
    // optimistic outgoing row; the wire body carries the matching marker so the
    // recipient replays it. Attachment sends leave this 0.
    effect: i32,
}

#[derive(Clone)]
struct PendingMedia {
    file_name: String,
    media_type: String,
    size_bytes: u64,
    is_image: bool,
    is_video: bool,
    is_audio: bool,
    // Local pixels for instant image preview while the upload is in flight.
    // None for non-image attachments.
    local_preview: Option<PicturePixels>,
}

/// One attachment queued in the composer (paperclip picker or clipboard
/// paste) but not yet sent. The bytes stay Rust-side; the UI only gets a
/// `StagedAttachment` chip row built by [`refresh_staged_ui`]. Nothing
/// uploads until the user presses Send — the visible chips *are* the
/// confirmation step.
#[derive(Clone)]
struct StagedFile {
    file_name: String,
    media_type: String,
    bytes: Vec<u8>,
    is_image: bool,
    // Full-resolution decode, reused as the optimistic bubble preview and
    // seeded into the attachment image cache once the upload confirms.
    preview: Option<PicturePixels>,
    // Small (≤96px) decode for the chip thumbnail, so rebuilding the chip
    // model never copies full screenshots around.
    thumb: Option<PicturePixels>,
}

#[derive(Clone)]
enum PendingReactionOp {
    /// I just clicked an emoji on a confirmed message — add a chip with
    /// `mine: true` unless the snapshot already shows my reaction.
    Add(String),
    /// I just unreacted — drop the `mine` flag and count from any chips on
    /// this target while the network catches up.
    Remove,
}

#[derive(Default)]
struct PendingState {
    /// group_hex → ordered list of pending outgoing messages. Append-only;
    /// entries are removed (or marked failed) when the send resolves.
    sends: HashMap<String, Vec<PendingSend>>,
    /// (group_hex, target_message_id_hex) → my latest pending reaction op
    /// on that target. Only one op per target at a time (the most recent
    /// click wins).
    reactions: HashMap<(String, String), PendingReactionOp>,
    /// (group_hex, target_message_id_hex) → the replacement text of my
    /// not-yet-confirmed edit of that message. Mirrors `reactions`: a single
    /// in-flight op per target; cleared when the kind-1009 send resolves.
    edits: HashMap<(String, String), String>,
}

impl PendingState {
    fn add_send(&mut self, group_hex: &str, send: PendingSend) {
        self.sends
            .entry(group_hex.to_string())
            .or_default()
            .push(send);
    }
    fn drop_send(&mut self, group_hex: &str, temp_id: &str) {
        if let Some(v) = self.sends.get_mut(group_hex) {
            v.retain(|p| p.temp_id != temp_id);
        }
    }
    fn mark_send_failed(&mut self, group_hex: &str, temp_id: &str) {
        if let Some(v) = self.sends.get_mut(group_hex) {
            for p in v.iter_mut() {
                if p.temp_id == temp_id {
                    p.failed = true;
                }
            }
        }
    }
    fn find_send(&self, group_hex: &str, temp_id: &str) -> Option<PendingSend> {
        self.sends
            .get(group_hex)
            .and_then(|v| v.iter().find(|p| p.temp_id == temp_id).cloned())
    }
}

// Monotonic temp-id source. Survives the lifetime of the process; we only
// need uniqueness within a session.
fn next_temp_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let v = N.fetch_add(1, Ordering::Relaxed);
    format!("pending:{v}")
}

// ─── Durable offline send queue ────────────────────────────────────────────
//
// The optimistic overlay above lives only in RAM. These process-wide handles
// add the missing durability + auto-flush-on-reconnect: see `offline_queue.rs`
// for the encrypted on-disk store. The *disk* queue is the source of truth for
// any (re)dispatch — the overlay is just what's rendered.

use std::collections::HashSet;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};

/// temp_ids whose send is currently in flight (dispatched, not yet resolved).
/// The reconnect flush skips these so a send can't be dispatched twice
/// concurrently. Entries are inserted at dispatch and removed when the op
/// resolves (ack or error), on whichever thread resolves it.
fn offline_inflight() -> &'static Mutex<HashSet<String>> {
    static S: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}

fn offline_inflight_insert(temp_id: &str) {
    if let Ok(mut s) = offline_inflight().lock() {
        s.insert(temp_id.to_string());
    }
}

fn offline_inflight_remove(temp_id: &str) {
    if let Ok(mut s) = offline_inflight().lock() {
        s.remove(temp_id);
    }
}

fn offline_inflight_contains(temp_id: &str) -> bool {
    offline_inflight()
        .lock()
        .map(|s| s.contains(temp_id))
        .unwrap_or(false)
}

/// Set by the background connectivity watcher when there's queued work to (re)try
/// — on first boot-ready and on every offline→online relay transition. The UI
/// timer consumes it and calls `flush_now`.
fn offline_flush_requested() -> &'static AtomicBool {
    static B: AtomicBool = AtomicBool::new(false);
    &B
}

/// Last-known connected relay count, published by the watcher thread so the
/// UI-thread flush can decide whether to dispatch (online) or only render the
/// queued bubbles (offline) without itself blocking on `relay_health`.
fn offline_last_connected() -> &'static AtomicUsize {
    static N: AtomicUsize = AtomicUsize::new(0);
    &N
}

/// Seal `send` into the durable queue using the session vault, if it's unlocked.
/// A no-op (best-effort) when the vault handle isn't present.
fn offline_persist(
    vault_cell: &Arc<Mutex<Option<Arc<Mutex<Vault>>>>>,
    send: &offline_queue::QueuedSend,
) {
    if let Some(vault) = vault_cell.lock().ok().and_then(|g| g.clone()) {
        offline_queue::put(&vault, send);
    }
}

/// Boot-only duplicate guard for the narrow kill-after-publish-before-ack
/// window: a queued send whose relay publish actually succeeded but whose
/// durable entry we never got to delete before the process exited. On the next
/// boot we'd otherwise re-send it. Returns true when an outgoing kind-9 from
/// `my_id` whose body matches one of `bodies` already exists near the enqueue
/// time (±10 min). Text-only — attachment bodies have no stable comparison key.
fn looks_already_sent(
    backend: &Backend,
    group_hex: &str,
    my_id: &str,
    bodies: &[String],
    enqueued_at: u64,
) -> bool {
    let Ok(msgs) = backend.messages(group_hex, Some(msg_window_for(group_hex))) else {
        return false;
    };
    msgs.iter().any(|m| {
        m.kind == 9
            && m.sender.eq_ignore_ascii_case(my_id)
            && bodies.iter().any(|b| &m.plaintext == b)
            && m.recorded_at.abs_diff(enqueued_at) <= 600
    })
}

// ─── Voice-message state ───────────────────────────────────────────────────

// The active cpal recorder and the currently-playing rodio audio player are
// !Send, so they live in thread-locals on the Slint UI thread. The timer
// thread only reads the monotonic start instant; it never touches the
// recorder. The monitor thread only touches the rodio Sink (Send + Sync).
thread_local! {
    static ACTIVE_AUDIO_RECORDER: RefCell<Option<audio::AudioRecorder>> = const { RefCell::new(None) };
    static ACTIVE_AUDIO_PLAYER: RefCell<Option<audio::AudioPlayer>> = const { RefCell::new(None) };
}

fn with_active_recorder<R>(f: impl FnOnce(&mut Option<audio::AudioRecorder>) -> R) -> R {
    ACTIVE_AUDIO_RECORDER.with(|r| f(&mut r.borrow_mut()))
}

fn with_active_player<R>(f: impl FnOnce(&mut Option<audio::AudioPlayer>) -> R) -> R {
    ACTIVE_AUDIO_PLAYER.with(|p| f(&mut p.borrow_mut()))
}

/// Start instant of the current recording, shared with the timer thread.
fn recording_start() -> &'static Mutex<Option<std::time::Instant>> {
    use std::sync::OnceLock;
    static S: OnceLock<Mutex<Option<std::time::Instant>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

/// The message id of the currently-playing voice message.
fn current_audio_message_id() -> &'static Mutex<Option<String>> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(None))
}

/// Last-known playback progress per message id (0..1). Kept so rows that
/// scroll out and back in show the correct progress without re-querying the
/// player.
fn audio_progress() -> &'static Mutex<HashMap<String, f32>> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<HashMap<String, f32>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Duration label per audio message id (e.g. "0:42"), captured the first
/// time the clip is decoded.
fn audio_meta() -> &'static Mutex<HashMap<String, String>> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

fn rgb(hex: u32) -> Color {
    Color::from_rgb_u8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

fn s(v: &str) -> SharedString {
    v.into()
}

/// Gate for setting a brand-new vault password. This password is the only thing
/// protecting every stored secret, and there is no recovery — so we require a
/// minimum length and a matching confirmation.
fn validate_new_password(pw: &str, confirm: &str) -> Result<(), String> {
    if pw.chars().count() < 8 {
        return Err("Password must be at least 8 characters.".to_string());
    }
    if pw != confirm {
        return Err("Passwords don't match.".to_string());
    }
    Ok(())
}

/// Localized snapshot of the user-facing error/status copy.
///
/// The strings themselves are authored as `@tr()` properties on the Slint
/// `ErrorCopy` global so they flow through the same gettext catalogs as the
/// rest of the UI (the project keeps *all* i18n in the Slint `@tr` catalogs —
/// see the `notification_body` note). But `friendly_error` and the relay-pane
/// status setters run on worker threads, where touching a Slint getter is
/// unsound. So we snapshot the translated strings on the UI thread (at startup
/// and on every locale change) into this process-global, and worker threads
/// read the snapshot instead.
#[derive(Clone)]
struct ErrorCopySnapshot {
    invalid_key: String,
    network: String,
    sync: String,
    backend: String,
    switch_account: String,
    accept: String,
    block: String,
    archive: String,
    unarchive: String,
    send: String,
    edit: String,
    react: String,
    unreact: String,
    kp_publish: String,
    kp_rotate: String,
    kp_refresh: String,
    republish: String,
    add_account: String,
    create_chat: String,
    add_contact: String,
    add_member: String,
    group_settings: String,
    group_image: String,
    save_profile: String,
    upload_picture: String,
    generic: String,
    not_connected: String,
    relay_already_listed: String,
    save_relays_failed: String,
    relay_added: String,
    relay_removed: String,
    republishing: String,
}

impl Default for ErrorCopySnapshot {
    /// English fallback, identical to the `@tr()` source strings. Used before
    /// the first UI-thread snapshot lands (and as a belt-and-braces default).
    fn default() -> Self {
        Self {
            invalid_key: "That doesn't look like a valid npub or public key. Double-check it and try again.".into(),
            network: "Can't reach your relays right now. Check your network and relay settings, then try again.".into(),
            sync: "Couldn't finish syncing. We'll keep retrying — check your relay settings if this keeps happening.".into(),
            backend: "Couldn't start up. Check your network and relay settings, then try again.".into(),
            switch_account: "Couldn't switch accounts. Please try again in a moment.".into(),
            accept: "Couldn't accept the invitation. Please try again in a moment.".into(),
            block: "Couldn't decline the invitation. Please try again in a moment.".into(),
            archive: "Couldn't archive this chat. Please try again.".into(),
            unarchive: "Couldn't restore this chat. Please try again.".into(),
            send: "Couldn't send your message. Check your connection and try again.".into(),
            edit: "Couldn't save your edit. Check your connection and try again.".into(),
            react: "Couldn't add your reaction. Please try again.".into(),
            unreact: "Couldn't remove your reaction. Please try again.".into(),
            kp_publish: "Couldn't publish your key package. Check your relay settings and try again.".into(),
            kp_rotate: "Couldn't rotate your key package. Check your relay settings and try again.".into(),
            kp_refresh: "Couldn't refresh your key packages. Check your relay settings and try again.".into(),
            republish: "Couldn't republish to your relays. Check your relay settings and try again.".into(),
            add_account: "Couldn't add that account. Please check the key and try again.".into(),
            create_chat: "Couldn't create the chat. Please try again.".into(),
            add_contact: "Couldn't add that contact. Please try again.".into(),
            add_member: "Couldn't add that member. Please try again.".into(),
            group_settings: "Couldn't update the group settings. Please try again.".into(),
            group_image: "Couldn't update the group image. Please try again.".into(),
            save_profile: "Couldn't save your profile. Check your connection and try again.".into(),
            upload_picture: "Couldn't upload your picture. Please try again.".into(),
            generic: "Something went wrong. Please try again.".into(),
            not_connected: "Not connected yet. Please wait a moment and try again.".into(),
            relay_already_listed: "That relay is already in your list.".into(),
            save_relays_failed: "Couldn't save your relay list. Please try again.".into(),
            relay_added: "Relay added.".into(),
            relay_removed: "Relay removed.".into(),
            republishing: "Republishing…".into(),
        }
    }
}

fn error_copy_cell() -> &'static Mutex<ErrorCopySnapshot> {
    static C: OnceLock<Mutex<ErrorCopySnapshot>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(ErrorCopySnapshot::default()))
}

/// Snapshot the localized `ErrorCopy` strings off the Slint global into the
/// process-global cache. MUST be called on the UI/event-loop thread (it reads
/// Slint property getters). Call at startup and after every locale change so
/// worker-thread error copy follows the active language.
fn refresh_error_copy(ui: &DarkMatterLinux) {
    let g = ui.global::<ErrorCopy>();
    let snap = ErrorCopySnapshot {
        invalid_key: g.get_invalid_key().to_string(),
        network: g.get_network().to_string(),
        sync: g.get_sync().to_string(),
        backend: g.get_backend().to_string(),
        switch_account: g.get_switch_account().to_string(),
        accept: g.get_accept().to_string(),
        block: g.get_block().to_string(),
        archive: g.get_archive().to_string(),
        unarchive: g.get_unarchive().to_string(),
        send: g.get_send().to_string(),
        edit: g.get_edit().to_string(),
        react: g.get_react().to_string(),
        unreact: g.get_unreact().to_string(),
        kp_publish: g.get_kp_publish().to_string(),
        kp_rotate: g.get_kp_rotate().to_string(),
        kp_refresh: g.get_kp_refresh().to_string(),
        republish: g.get_republish().to_string(),
        add_account: g.get_add_account().to_string(),
        create_chat: g.get_create_chat().to_string(),
        add_contact: g.get_add_contact().to_string(),
        add_member: g.get_add_member().to_string(),
        group_settings: g.get_group_settings().to_string(),
        group_image: g.get_group_image().to_string(),
        save_profile: g.get_save_profile().to_string(),
        upload_picture: g.get_upload_picture().to_string(),
        generic: g.get_generic().to_string(),
        not_connected: g.get_not_connected().to_string(),
        relay_already_listed: g.get_relay_already_listed().to_string(),
        save_relays_failed: g.get_save_relays_failed().to_string(),
        relay_added: g.get_relay_added().to_string(),
        relay_removed: g.get_relay_removed().to_string(),
        republishing: g.get_republishing().to_string(),
    };
    *error_copy_cell().lock().unwrap() = snap;
}

/// Read the current localized `ErrorCopy` snapshot. Safe from any thread.
fn error_copy() -> ErrorCopySnapshot {
    error_copy_cell().lock().unwrap().clone()
}

/// Map a low-level backend error into approachable, action-oriented UI copy.
///
/// User-facing error surfaces must never show raw `anyhow` context strings,
/// Rust debug formatting, or internal module/concept names. The full technical
/// error is still logged at every call site (`eprintln!("[op] {e:#}")`) and
/// stays available for diagnosis — this governs only what the *user* reads.
///
/// Classification is two-tier: first we inspect the flattened error chain for
/// signals that point at a specific, fixable user action (a malformed key, an
/// unreachable relay); failing that we fall back to a reassuring, operation-
/// specific message. `op` is the short internal label already used at the call
/// site (e.g. "sync", "switch account", "send").
///
/// The returned text is localized: it comes from the `ErrorCopy` snapshot which
/// mirrors the Slint `@tr()` catalogs for the active locale.
fn friendly_error(op: &str, e: &anyhow::Error) -> String {
    let copy = error_copy();
    // Flatten the whole error chain once for case-insensitive keyword matching.
    let detail = format!("{e:#}").to_lowercase();

    // Tier 1 — content-based classification. These conditions name a concrete
    // thing the user can fix, so they take priority over the op default.
    if detail.contains("npub") || detail.contains("pubkey") || detail.contains("public key") {
        return copy.invalid_key;
    }
    if detail.contains("timed out")
        || detail.contains("timeout")
        || detail.contains("connection")
        || detail.contains("connect")
        || detail.contains("network")
        || detail.contains("relay")
        || detail.contains("offline")
        || detail.contains("unreachable")
        || detail.contains("dns")
    {
        return copy.network;
    }

    // Tier 2 — operation-specific fallback. Reassuring, names no internals.
    match op {
        "sync" => copy.sync,
        "backend" => copy.backend,
        "switch account" => copy.switch_account,
        "accept" => copy.accept,
        "block" => copy.block,
        "archive" => copy.archive,
        "unarchive" => copy.unarchive,
        "send" => copy.send,
        "edit" => copy.edit,
        "react" => copy.react,
        "unreact" => copy.unreact,
        "kp_publish" => copy.kp_publish,
        "kp_rotate" => copy.kp_rotate,
        "kp_refresh" => copy.kp_refresh,
        "republish" => copy.republish,
        "add account" => copy.add_account,
        "create chat" => copy.create_chat,
        "add contact" => copy.add_contact,
        "add member" => copy.add_member,
        "group settings" => copy.group_settings,
        "group image" => copy.group_image,
        "save profile" => copy.save_profile,
        "upload picture" => copy.upload_picture,
        _ => copy.generic,
    }
}

fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(v))
}

/// Recompute the breadcrumb from the UI's own models. Same effect as the
/// `refresh_breadcrumb` closure in `main`, but callable from `Send` completion
/// closures that can't capture the model handles.
fn refresh_breadcrumb_now(ui: &DarkMatterLinux) {
    ui.set_breadcrumb(breadcrumb(
        ui.get_active_page(),
        &ui.get_chats(),
        &ui.get_contacts(),
        &ui.get_archived_chats(),
        ui.get_active_chat(),
        ui.get_active_contact(),
        ui.get_active_archived(),
    ));
}

fn breadcrumb(
    page: i32,
    chats: &ModelRc<ChatMeta>,
    contacts: &ModelRc<Contact>,
    archived: &ModelRc<ArchivedChat>,
    active_chat: i32,
    active_contact: i32,
    active_archived: i32,
) -> SharedString {
    let label = match page {
        0 => chats
            .row_data(active_chat as usize)
            .map(|c| c.name.to_string())
            .unwrap_or_default(),
        1 => contacts
            .row_data(active_contact as usize)
            .map(|c| c.name.to_string())
            .unwrap_or_default(),
        2 => archived
            .row_data(active_archived as usize)
            .map(|c| c.name.to_string())
            .unwrap_or_default(),
        3 => "Keys".into(),
        4 => "Settings".into(),
        _ => "Profile".into(),
    };
    label.to_uppercase().into()
}

// Pages the UI can show. Cast to i32 for Slint's `active-page` property.
#[repr(i32)]
#[derive(Copy, Clone)]
enum Page {
    Chats = 0,
    Contacts = 1,
    Archived = 2,
    Keys = 3,
    Settings = 4,
    Profile = 5,
}

// Master list of palette actions. Each has an id (used by Rust to dispatch),
// a label (shown), a group header, and an optional keyboard hint chip.
fn all_palette_actions() -> Vec<PaletteAction> {
    let mk = |id: &str, label: &str, group: &str, kbd: &str| PaletteAction {
        id: s(id),
        label: s(label),
        group: s(group),
        kbd: s(kbd),
    };
    vec![
        mk("nav.chats", "Go to Chats", "NAVIGATE", "1"),
        mk("nav.contacts", "Go to Contacts", "NAVIGATE", "2"),
        mk("nav.archived", "Go to Archived", "NAVIGATE", "3"),
        mk("nav.keys", "Go to Keys", "NAVIGATE", "4"),
        mk("nav.settings", "Go to Settings", "NAVIGATE", "5"),
        mk("nav.profile", "Go to Profile", "NAVIGATE", ""),
        mk("act.new-chat", "New chat", "ACTIONS", "Ctrl N"),
        mk("act.copy-npub", "Copy your npub", "ACTIONS", ""),
        mk("act.toggle-retro", "Toggle retro mode", "ACTIONS", ""),
    ]
}

fn filter_palette(all: &[PaletteAction], query: &str) -> Vec<PaletteAction> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return all.to_vec();
    }
    all.iter()
        .filter(|a| a.label.to_lowercase().contains(&q) || a.id.to_lowercase().contains(&q))
        .cloned()
        .collect()
}

fn normalize_locale(code: &str) -> &'static str {
    let base = code
        .split('.')
        .next()
        .unwrap_or(code)
        .split('_')
        .next()
        .unwrap_or(code);
    match base {
        "it" => "it",
        "de" => "de",
        "ja" => "ja",
        _ => "en",
    }
}

fn normalize_theme_mode(mode: &str) -> &'static str {
    match mode {
        "light" => "light",
        "retro" => "retro",
        "terminal" => "terminal",
        "crayon" => "crayon",
        "synthwave" => "synthwave",
        "chalkboard" => "chalkboard",
        _ => "dark",
    }
}

fn normalize_accent_color(color: &str) -> &'static str {
    match color {
        "ocean" => "ocean",
        "berry" => "berry",
        "coral" => "coral",
        "lavender" => "lavender",
        _ => "mint",
    }
}

fn accent_color_idx(color: &str) -> i32 {
    match color {
        "ocean" => 1,
        "berry" => 2,
        "coral" => 3,
        "lavender" => 4,
        _ => 0,
    }
}

fn apply_theme_mode(ui: &DarkMatterLinux, mode: &str) {
    let mode = normalize_theme_mode(mode);
    ui.set_light_theme(mode == "light");
    ui.set_retro_mode(mode == "retro");
    ui.set_terminal_mode(mode == "terminal");
    ui.set_crayon_mode(mode == "crayon");
    ui.set_synthwave_mode(mode == "synthwave");
    ui.set_chalkboard_mode(mode == "chalkboard");
}

fn locale_display(code: &str) -> &'static str {
    match normalize_locale(code) {
        "it" => "Italiano",
        "de" => "Deutsch",
        "ja" => "日本語",
        _ => "English",
    }
}

fn apply_locale(locale: &str) {
    let code = normalize_locale(locale);
    if let Err(e) = slint::select_bundled_translation(code) {
        eprintln!("[i18n] select_bundled_translation({code}): {e}");
        let _ = slint::select_bundled_translation("en");
    }
}

fn main() -> Result<(), slint::PlatformError> {
    // marmot crates emit `tracing` events; install a subscriber so RUST_LOG works.
    // Default to info if RUST_LOG isn't set.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    let ui = DarkMatterLinux::new()?;

    // Settings (locale + theme + accent + debug toggle) — load early so
    // bundled translations apply before the user sees any @tr()-annotated UI.
    let mut initial_settings = Settings::load();
    let locale = normalize_locale(&initial_settings.locale).to_string();
    initial_settings.locale = locale.clone();
    let theme_mode = normalize_theme_mode(&initial_settings.theme).to_string();
    initial_settings.theme = theme_mode.clone();
    let accent_color = normalize_accent_color(&initial_settings.accent_color);
    initial_settings.accent_color = accent_color.to_string();
    apply_locale(&locale);
    ui.set_locale(s(&locale));
    ui.set_locale_display(s(locale_display(&locale)));
    // Snapshot the localized error/status copy off the Slint `ErrorCopy` global
    // now that the locale is applied, so worker threads have it from the start.
    refresh_error_copy(&ui);
    apply_theme_mode(&ui, &theme_mode);
    ui.set_accent_color(accent_color_idx(accent_color));
    ui.set_outgoing_on_right(initial_settings.outgoing_on_right);
    // Drives ⌘-vs-Ctrl shortcut hints (command palette badge, etc.).
    ui.set_is_macos(cfg!(target_os = "macos"));
    apply_stamp_formats(&initial_settings);
    ui.set_time_format(s(&initial_settings.time_format));
    ui.set_date_format(s(&initial_settings.date_format));
    ui.set_notifications_enabled(initial_settings.notifications_enabled);
    ui.set_notification_sound(initial_settings.notification_sound);
    ui.set_notification_preview(initial_settings.notification_preview);
    // Live notification state shared with the chat watcher (which runs on the
    // tokio thread, so it can't reach the Rc<RefCell<Settings>>). The toggle
    // callbacks keep both in sync.
    let notif = Arc::new(notify::NotifState::new(
        initial_settings.notifications_enabled,
        initial_settings.notification_sound,
        initial_settings.notification_preview,
        initial_settings.muted_chats.clone(),
    ));
    let settings_cell: Rc<RefCell<Settings>> = Rc::new(RefCell::new(initial_settings));

    // All models start empty; they're filled from marmot-app after login.
    let contacts: ModelRc<Contact> = ModelRc::new(VecModel::from(Vec::<Contact>::new()));
    let archived: ModelRc<ArchivedChat> = ModelRc::new(VecModel::from(Vec::<ArchivedChat>::new()));
    let chats: ModelRc<ChatMeta> = ModelRc::new(VecModel::from(Vec::<ChatMeta>::new()));
    let chats_messages: ModelRc<ModelRc<ChatMessage>> =
        ModelRc::new(VecModel::from(Vec::<ModelRc<ChatMessage>>::new()));
    ui.set_contacts(contacts.clone());
    ui.set_archived_chats(archived.clone());
    ui.set_chats(chats.clone());
    ui.set_chats_messages(chats_messages.clone());
    ui.set_my_npub(s(""));

    // Backend handle, populated after a successful login. We store the active
    // group id parallel to the chats model so on_send_message can resolve it.
    // group_ids is Arc<Mutex<…>> so the chat watcher (running on tokio) can
    // append to it before bouncing into the Slint event loop.
    // `Arc<Mutex>` (not `Rc<RefCell>`) because boot runs on a worker thread
    // and installs the result into this cell from inside
    // `slint::invoke_from_event_loop`. Access from UI callbacks is always
    // single-threaded — `lock()` is uncontended.
    // The inner `Arc<Backend>` lets worker threads clone a handle and drop
    // the lock *before* a blocking call, so the UI thread never contends on
    // this mutex while a relay round-trip is in flight.
    let backend_cell: Arc<Mutex<Option<Arc<Backend>>>> = Arc::new(Mutex::new(None));
    // The unlocked secret vault for this session. Held behind `Arc<Mutex>` so a
    // clone can be moved into the boot worker thread (and into marmot's secret
    // store) while the UI thread keeps its own handle. `None` until the user
    // unlocks or creates a vault on the login screen.
    // `Arc<Mutex>` (not `Rc<RefCell>`) so the boot closure stays `Send` and
    // can be invoked from worker-thread completion closures.
    let vault_cell: Arc<Mutex<Option<Arc<Mutex<Vault>>>>> = Arc::new(Mutex::new(None));
    let group_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let archived_group_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    // Optimistic-render overlay: pending sends + pending reactions. Lives
    // behind `Arc<Mutex<…>>` because async send/react callbacks fire on the
    // tokio worker thread and need to mutate it before hopping back to the
    // Slint event loop via `invoke_from_event_loop` (which requires Send).
    let pending_state: Arc<Mutex<PendingState>> = Arc::new(Mutex::new(PendingState::default()));
    // Attachments queued in the composer, awaiting an explicit Send. Global
    // like the draft itself (survives chat switches, cleared on account
    // switch); the chip row keeps it visible wherever the composer is.
    let staged_files: Arc<Mutex<Vec<StagedFile>>> = Arc::new(Mutex::new(Vec::new()));

    // Stash the handles album auto-load needs so the (pure) row builders can
    // kick off downloads for not-yet-cached album images. Set once, read only
    // on the UI thread — see `maybe_autoload_album`.
    set_album_load_ctx(AlbumLoadCtx {
        weak: ui.as_weak(),
        backend_cell: backend_cell.clone(),
        vault_cell: vault_cell.clone(),
        group_ids: group_ids.clone(),
        pending_state: pending_state.clone(),
    });
    // Currently-active per-chat message watcher. Aborted and replaced when the
    // user switches chats so we never leak background tasks.
    // `Arc<Mutex>` (not `Rc<RefCell>`) so the handle cell can ride into the
    // async chat-switch completion that installs the watcher after the
    // off-thread snapshot fetch lands.
    let active_message_watcher: Arc<Mutex<Option<JoinHandle<()>>>> = Arc::new(Mutex::new(None));
    // The chat-list watcher for the *active account*. Its subscription is
    // bound to the account label it was created with, so on account switch it
    // must be aborted and re-installed — otherwise the previous account's
    // chat updates keep flowing into the (now repopulated) models.
    let chats_watcher: Arc<Mutex<Option<JoinHandle<()>>>> = Arc::new(Mutex::new(None));

    // ─── Login gate ────────────────────────────────────────────────────
    // Holds the freshly generated nsec until the user confirms they've saved it.
    let pending_generated: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // Account id (hex) of a key generated this session whose starter profile
    // hasn't been published yet. Checked on every boot success — it survives
    // the relays-added first-run reboot (publishing fails while no relays are
    // configured) and is cleared only once the kind-0 actually lands.
    let pending_profile_seed: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // Display name picked at key-generation time — reused when seeding the
    // kind-0 so the login preview matches the published profile.
    let pending_profile_name: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // Chats whose encryption banner entrance has already played.
    let encryption_banner_seen: Arc<Mutex<std::collections::HashSet<String>>> =
        Arc::new(Mutex::new(std::collections::HashSet::new()));

    // Boot the backend from an nsec and populate the chat models. Errors are
    // surfaced on the UI's backend-error property; the UI stays logged-in
    // either way so the user can still navigate.
    // A plain closure (not `Rc<dyn Fn>`): every capture is `Send + Clone`, so
    // clones can ride through worker threads back into
    // `invoke_from_event_loop` completions (login/unlock run the vault KDF
    // off-thread and boot from the completion).
    let boot_backend = {
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        let archived_group_ids = archived_group_ids.clone();
        let vault_cell = vault_cell.clone();
        let chats_watcher = chats_watcher.clone();
        let notif = notif.clone();
        let pending_profile_seed = pending_profile_seed.clone();
        let pending_profile_name = pending_profile_name.clone();
        let encryption_banner_seen = encryption_banner_seen.clone();
        // `active_hint` names the account (id hex) to display first — the
        // vault-recorded last-active account on unlock, `None` on first run.
        move |nsec: String, vault: Arc<Mutex<Vault>>, active_hint: Option<String>| {
            let Some(ui) = weak.upgrade() else { return };
            // Keep the unlocked vault for the rest of the session.
            *vault_cell.lock().unwrap() = Some(vault.clone());
            ui.set_backend_ready(false);
            ui.set_backend_error(s(""));
            ui.set_booting(true);
            ui.set_booting_phase(0);
            ui.set_booting_status(s("Opening vault…"));

            // Hand the boot off to a worker thread so the Slint event loop
            // keeps rendering the splash screen. Send the result back via
            // invoke_from_event_loop. Capture only Send data — model handles
            // are `Rc`-based (!Send), so we look them up off the UI handle
            // inside the invoke closure instead.
            let weak_for_worker = weak.clone();
            let backend_cell = backend_cell.clone();
            let group_ids = group_ids.clone();
            let archived_group_ids = archived_group_ids.clone();
            let chats_watcher = chats_watcher.clone();
            let notif = notif.clone();
            let pending_profile_seed = pending_profile_seed.clone();
            let pending_profile_name = pending_profile_name.clone();
            let encryption_banner_seen = encryption_banner_seen.clone();
            std::thread::spawn(move || {
                let relays = backend::load_relays();
                // Kept aside for the per-account nsec migration write below —
                // `secret_store` consumes the primary handle.
                let vault_for_migrate = vault.clone();
                // marmot's per-account secret store reads/writes the same vault.
                let secret_store = Arc::new(vault::VaultSecretStore::new(vault));
                // Fires when boot's background network phase (directory sync,
                // KP bootstrap, inbox catch-up) completes — possibly tens of
                // seconds after the UI is already interactive, e.g. when a
                // relay eats its full connection timeout. One non-destructive
                // refresh picks up whatever the sync pulled in without
                // yanking an already-open chat out from under the user.
                // Set once the background sync's refresh has run; stops the
                // early upgrade polls scheduled below.
                let sync_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let weak_for_sync = weak_for_worker.clone();
                let backend_cell_for_sync = backend_cell.clone();
                let group_ids_for_sync = group_ids.clone();
                let archived_for_sync = archived_group_ids.clone();
                let sync_done_for_sync = sync_done.clone();
                let weak_for_status = weak_for_worker.clone();
                let on_status: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(move |msg: &str| {
                    let weak = weak_for_status.clone();
                    let msg = msg.to_string();
                    let phase = boot_phase_for_status(&msg);
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = weak.upgrade() else { return };
                        ui.set_booting_status(msg.into());
                        ui.set_booting_phase(phase);
                    });
                });
                let on_synced = move |sync_result: anyhow::Result<()>| {
                    let _ = slint::invoke_from_event_loop(move || {
                        sync_done_for_sync.store(true, std::sync::atomic::Ordering::Relaxed);
                        let Some(ui) = weak_for_sync.upgrade() else {
                            return;
                        };
                        if let Err(e) = sync_result {
                            eprintln!("[backend] background sync failed: {e:#}");
                            ui.set_backend_error(friendly_error("sync", &e).into());
                            return;
                        }
                        let Some(b) = backend_cell_for_sync.lock().unwrap().clone() else {
                            return;
                        };
                        // The directory sync just finished — re-pull every
                        // cached name/picture so changes made while we were
                        // offline converge (async; next rebuilds pick them up).
                        b.refresh_all_profiles_async();
                        // Every list refresh below fetches on the backend
                        // runtime and applies back on the UI thread — this
                        // closure does zero sqlite/disk reads itself.
                        merge_chat_list_rows_async(&ui, &b, &group_ids_for_sync);
                        refresh_contacts_async(&ui, &b, |_| {});
                        refresh_archived_async(&ui, &b, &archived_for_sync);
                        populate_profile_async(&ui, &b);
                        refresh_kp_local_async(&ui, &b);
                        refresh_network_post_boot(&b, &ui);
                        // The profile refreshes queued above land asynchronously
                        // AFTER this merge — one delayed, change-only merge picks
                        // them up (no-op rows stay untouched, so this is
                        // visually free).
                        let weak2 = weak_for_sync.clone();
                        let backend_cell2 = backend_cell_for_sync.clone();
                        let group_ids2 = group_ids_for_sync.clone();
                        slint::Timer::single_shot(
                            std::time::Duration::from_millis(1_500),
                            move || {
                                let Some(ui) = weak2.upgrade() else { return };
                                let Some(b) = backend_cell2.lock().unwrap().clone() else {
                                    return;
                                };
                                merge_chat_list_rows_async(&ui, &b, &group_ids2);
                            },
                        );
                    });
                };
                let result = Backend::boot(
                    &nsec,
                    relays,
                    secret_store,
                    active_hint,
                    on_synced,
                    Some(on_status),
                );
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak_for_worker.upgrade() else {
                        return;
                    };
                    match result {
                        Ok(b) => {
                            let b = Arc::new(b);
                            // Every list is fetched on the backend runtime and
                            // applied back on the UI thread — the boot closure
                            // itself does zero sqlite/disk reads.
                            populate_models_for_active(&ui, &b, &group_ids, &archived_group_ids);
                            // First chat may already be visible — play the
                            // encryption-banner entrance once its key is known.
                            let weak_banner = ui.as_weak();
                            let group_ids_banner = group_ids.clone();
                            let banner_seen_boot = encryption_banner_seen.clone();
                            slint::Timer::single_shot(
                                std::time::Duration::from_millis(350),
                                move || {
                                    let Some(ui) = weak_banner.upgrade() else {
                                        return;
                                    };
                                    let idx = ui.get_active_chat() as usize;
                                    let key = group_ids_banner.lock().unwrap().get(idx).cloned();
                                    trigger_encryption_banner_entrance(
                                        &ui,
                                        key.as_deref(),
                                        &banner_seen_boot,
                                    );
                                },
                            );
                            // The displayed account may be the vault's
                            // last-active hint rather than the nsec we booted
                            // with — derive the identity-bound chrome from the
                            // backend's actual active account.
                            let active = b.account();
                            if let Ok(npub) = npub_for_account_id(&active.account_id_hex) {
                                ui.set_my_qr(qr_image(&format!("nostr:{npub}")));
                                ui.set_my_npub(npub.into());
                            }
                            refresh_accounts_model(&ui, &b);
                            // Older vaults only carry the bare "nsec" entry —
                            // backfill the per-account key for the boot
                            // account so every account is stored uniformly.
                            if let Ok(keys) = Keys::parse(&nsec) {
                                let key = vault::nsec_key_for(&keys.public_key().to_hex());
                                let nsec = nsec.clone();
                                let vault = vault_for_migrate.clone();
                                std::thread::spawn(move || {
                                    let mut v = vault.lock().unwrap();
                                    if !v.has(&key)
                                        && let Err(e) = v.set(&key, &nsec)
                                    {
                                        eprintln!("[vault] migrate {key} failed: {e}");
                                    }
                                });
                            }
                            install_chat_watcher(
                                &b,
                                ui.as_weak(),
                                group_ids.clone(),
                                backend_cell.clone(),
                                notif.clone(),
                                now_unix_secs(),
                                &chats_watcher,
                            );
                            *backend_cell.lock().unwrap() = Some(b.clone());
                            ui.set_backend_ready(true);
                            ui.set_booting(false);
                            // A key generated this session has no kind-0 yet —
                            // seed it with a random "[Adjective] [Animal]"
                            // name so the user shows up as something
                            // friendlier than a hex tail.
                            let seeding = pending_profile_seed.lock().unwrap().clone();
                            if seeding.as_deref() == Some(active.account_id_hex.as_str()) {
                                let cell = pending_profile_seed.clone();
                                let preset_name = pending_profile_name.lock().unwrap().take();
                                publish_random_profile_async(
                                    &b,
                                    active.label.clone(),
                                    active.account_id_hex.clone(),
                                    preset_name,
                                    ui.as_weak(),
                                    move || *cell.lock().unwrap() = None,
                                );
                            }
                            // The background sync can take a relay's full
                            // connection timeout (~35s on a misbehaving
                            // relay) to *complete*, but the healthy relays
                            // deliver directory data within a couple of
                            // seconds. Poll a few light in-place merges so
                            // names/pictures/previews upgrade as soon as the
                            // cache warms instead of when the sync ends.
                            for delay_ms in [2_000u64, 6_000, 15_000] {
                                let weak = ui.as_weak();
                                let backend_cell = backend_cell.clone();
                                let group_ids = group_ids.clone();
                                let sync_done = sync_done.clone();
                                slint::Timer::single_shot(
                                    std::time::Duration::from_millis(delay_ms),
                                    move || {
                                        if sync_done.load(std::sync::atomic::Ordering::Relaxed) {
                                            return;
                                        }
                                        let Some(ui) = weak.upgrade() else { return };
                                        let Some(b) = backend_cell.lock().unwrap().clone() else {
                                            return;
                                        };
                                        merge_chat_list_rows_async(&ui, &b, &group_ids);
                                        refresh_contacts_async(&ui, &b, |_| {});
                                        populate_profile_async(&ui, &b);
                                    },
                                );
                            }
                        }
                        Err(e) => {
                            eprintln!("[backend] boot failed: {e:#}");
                            ui.set_backend_error(friendly_error("backend", &e).into());
                            ui.set_booting(false);
                        }
                    }
                });
            });
        }
    };

    // ─── Account switching ─────────────────────────────────────────────
    // Swap the displayed account: stop the per-account watchers, drop the
    // optimistic overlay and all per-account models *synchronously* (so a
    // stray send can't resolve an index against the previous account's group
    // list), then rebuild everything from the new account's snapshots. All
    // accounts keep their background sessions — this is a view change, not a
    // re-login. `Arc<dyn Fn + Send + Sync>` (not `Rc`) so the add-account
    // completion — which hops through a tokio worker before
    // `invoke_from_event_loop` — can carry a handle; it is only ever
    // *invoked* on the UI thread.
    let do_switch_account: Arc<dyn Fn(String) + Send + Sync> = {
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let vault_cell = vault_cell.clone();
        let group_ids = group_ids.clone();
        let archived_group_ids = archived_group_ids.clone();
        let pending_state = pending_state.clone();
        let staged_files = staged_files.clone();
        let active_message_watcher = active_message_watcher.clone();
        let chats_watcher = chats_watcher.clone();
        let notif = notif.clone();
        Arc::new(move |account_id: String| {
            let Some(ui) = weak.upgrade() else { return };
            let Some(backend) = backend_cell.lock().unwrap().clone() else {
                return;
            };
            if backend
                .account()
                .account_id_hex
                .eq_ignore_ascii_case(&account_id)
            {
                ui.set_show_account_switcher(false);
                return;
            }
            let summary = match backend.set_active_account(&account_id) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[accounts] switch failed: {e:#}");
                    ui.set_backend_error(friendly_error("switch account", &e).into());
                    return;
                }
            };
            // Remember the choice for the next unlock.
            if let Some(vault) = vault_cell.lock().unwrap().clone() {
                vault_set_async(
                    &vault,
                    vault::ACTIVE_ACCOUNT_KEY.to_string(),
                    summary.account_id_hex.to_ascii_lowercase(),
                );
            }
            // Stop the previous account's streams before the models change
            // under them, and drop its optimistic overlay outright.
            if let Some(h) = active_message_watcher.lock().unwrap().take() {
                h.abort();
            }
            if let Some(h) = chats_watcher.lock().unwrap().take() {
                h.abort();
            }
            *pending_state.lock().unwrap() = PendingState::default();
            // Clear every per-account model + selection synchronously so
            // nothing can act on stale rows while the rebuild is in flight.
            group_ids.lock().unwrap().clear();
            archived_group_ids.lock().unwrap().clear();
            if let Some(vm) = ui.get_chats().as_any().downcast_ref::<VecModel<ChatMeta>>() {
                vm.set_vec(Vec::new());
            }
            if let Some(vm) = ui
                .get_chats_messages()
                .as_any()
                .downcast_ref::<VecModel<ModelRc<ChatMessage>>>()
            {
                vm.set_vec(Vec::new());
            }
            if let Some(vm) = ui
                .get_contacts()
                .as_any()
                .downcast_ref::<VecModel<Contact>>()
            {
                vm.set_vec(Vec::new());
            }
            if let Some(vm) = ui
                .get_archived_chats()
                .as_any()
                .downcast_ref::<VecModel<ArchivedChat>>()
            {
                vm.set_vec(Vec::new());
            }
            ui.set_active_chat(0);
            ui.set_active_contact(0);
            ui.set_active_archived(0);
            ui.set_active_page(0);
            ui.set_show_chat_members(false);
            ui.set_messages_has_older(false);
            ui.set_composer_draft(s(""));
            staged_files.lock().unwrap().clear();
            refresh_staged_ui(&ui, &[]);
            ui.set_reply_target_id(s(""));
            ui.set_reply_target_author(s(""));
            ui.set_reply_target_preview(s(""));
            ui.set_editing_message_id(s(""));
            if let Ok(mut slot) = active_group_slot().lock() {
                slot.clear();
            }
            // Identity-bound chrome for the new account.
            if let Ok(npub) = npub_for_account_id(&summary.account_id_hex) {
                ui.set_my_qr(qr_image(&format!("nostr:{npub}")));
                ui.set_my_npub(npub.into());
            }
            // Reset the avatar to the new account's deterministic fallback;
            // populate_profile_async upgrades it once the profile loads.
            ui.set_my_av_has_picture(false);
            ui.set_my_av_picture(slint::Image::default());
            set_my_avatar(&ui, &backend);
            refresh_breadcrumb_now(&ui);
            // Rebuild from the new account's snapshots and re-subscribe.
            populate_models_for_active(&ui, &backend, &group_ids, &archived_group_ids);
            install_chat_watcher(
                &backend,
                ui.as_weak(),
                group_ids.clone(),
                backend_cell.clone(),
                notif.clone(),
                now_unix_secs(),
                &chats_watcher,
            );
            refresh_accounts_model(&ui, &backend);
            ui.set_show_account_switcher(false);
        })
    };

    ui.on_account_switcher_requested({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(b) = backend_cell.lock().unwrap().clone() else {
                return;
            };
            refresh_accounts_model(&ui, &b);
            ui.set_show_account_switcher(true);
        }
    });

    ui.on_switch_account({
        let do_switch = do_switch_account.clone();
        move |id| do_switch(id.to_string())
    });

    ui.on_add_account_requested({
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_show_account_switcher(false);
            ui.set_add_account_nsec(s(""));
            ui.set_add_account_status(s(""));
            ui.set_add_account_generated(false);
            ui.set_add_account_busy(false);
            ui.set_show_add_account(true);
        }
    });

    ui.on_add_account_dismissed({
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_show_add_account(false);
            ui.set_add_account_nsec(s(""));
            ui.set_add_account_generated(false);
            ui.set_add_account_status(s(""));
        }
    });

    ui.on_generate_add_account_key({
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let keys = Keys::generate();
            match keys.secret_key().to_bech32() {
                Ok(nsec) => {
                    ui.set_add_account_nsec(nsec.into());
                    ui.set_add_account_generated(true);
                    ui.set_add_account_status(s(""));
                }
                Err(e) => ui.set_add_account_status(format!("Failed to encode key: {e}").into()),
            }
        }
    });

    ui.on_add_account({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let vault_cell = vault_cell.clone();
        let do_switch = do_switch_account.clone();
        move |nsec_input| {
            let Some(ui) = weak.upgrade() else { return };
            let raw = nsec_input.trim().to_string();
            let Ok(keys) = Keys::parse(&raw) else {
                ui.set_add_account_status(s("That doesn't look like a valid nsec."));
                return;
            };
            let Some(backend) = backend_cell.lock().unwrap().clone() else {
                ui.set_add_account_status(s("Backend isn't ready yet."));
                return;
            };
            // Canonical bech32 form for vault storage, whatever was pasted.
            let nsec = match keys.secret_key().to_bech32() {
                Ok(n) => n,
                Err(e) => {
                    ui.set_add_account_status(format!("Failed to encode key: {e}").into());
                    return;
                }
            };
            let account_id = keys.public_key().to_hex();
            // A key generated in this dialog can't have a profile yet; a
            // pasted one may — only generated keys get a random starter name.
            let generated = ui.get_add_account_generated();
            ui.set_add_account_busy(true);
            ui.set_add_account_status(s(""));
            let weak = ui.as_weak();
            let vault_cell = vault_cell.clone();
            let do_switch = do_switch.clone();
            let backend_for_seed = backend.clone();
            backend.add_account_async(nsec.clone(), move |result| {
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    ui.set_add_account_busy(false);
                    match result {
                        Ok(summary) => {
                            // Seal the new key into the session vault so the
                            // account survives restarts (marmot's own secret
                            // landed there too, via VaultSecretStore).
                            if let Some(vault) = vault_cell.lock().unwrap().clone() {
                                vault_set_async(
                                    &vault,
                                    vault::nsec_key_for(&account_id),
                                    nsec.clone(),
                                );
                            }
                            ui.set_show_add_account(false);
                            ui.set_add_account_nsec(s(""));
                            ui.set_add_account_generated(false);
                            if generated {
                                publish_random_profile_async(
                                    &backend_for_seed,
                                    summary.label.clone(),
                                    summary.account_id_hex.clone(),
                                    None,
                                    ui.as_weak(),
                                    || {},
                                );
                            }
                            do_switch(summary.account_id_hex);
                        }
                        Err(e) => {
                            eprintln!("[add-account] {e:#}");
                            ui.set_add_account_status(friendly_error("add account", &e).into());
                        }
                    }
                });
            });
        }
    });

    // There is no silent auto-login anymore: secrets live in a password-encrypted
    // vault. If a vault exists, open on the Unlock screen (mode 3); otherwise the
    // first-run "choose" screen (mode 0). The vault is only decrypted once the
    // user supplies the password.
    if vault::exists() {
        ui.set_login_mode(3);
    } else {
        ui.set_login_mode(0);
    }

    // First run, existing nsec: validate the key + new password, create the vault,
    // seal the nsec into it, then boot.
    ui.on_login_with_nsec({
        let weak = ui.as_weak();
        let boot = boot_backend.clone();
        move |input, password, confirm| {
            let Some(ui) = weak.upgrade() else { return };
            let trimmed = input.trim().to_string();
            let password = password.to_string();
            // Cheap validation stays here so typos fail instantly; the
            // Argon2id KDF inside `Vault::create` is deliberately slow, so it
            // runs on a worker thread and the busy state gets a frame to paint.
            if let Err(err) = validate_new_password(&password, confirm.as_str()) {
                ui.set_login_error(err.into());
                return;
            }
            let Ok(keys) = Keys::parse(&trimmed) else {
                ui.set_login_error(s("That doesn't look like a valid nsec."));
                return;
            };
            ui.set_login_busy(true);
            let weak = weak.clone();
            let boot = boot.clone();
            std::thread::spawn(move || {
                let result = (|| -> Result<(String, String, Arc<Mutex<Vault>>), String> {
                    let npub = keys.public_key().to_bech32().map_err(|e| e.to_string())?;
                    let nsec = keys.secret_key().to_bech32().map_err(|e| e.to_string())?;
                    let mut v =
                        Vault::create(&password).map_err(|e| format!("create vault: {e}"))?;
                    v.set(vault::NSEC_KEY, &nsec)
                        .map_err(|e| format!("seal nsec: {e}"))?;
                    Ok((npub, nsec, Arc::new(Mutex::new(v))))
                })();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    ui.set_login_busy(false);
                    match result {
                        Ok((npub, nsec, vault)) => {
                            ui.set_login_error(s(""));
                            ui.set_my_qr(qr_image(&format!("nostr:{npub}")));
                            ui.set_my_npub(npub.into());
                            ui.set_login_nsec_input(s(""));
                            ui.set_password_input(s(""));
                            ui.set_password_confirm(s(""));
                            ui.set_logged_in(true);
                            boot(nsec, vault, None);
                        }
                        Err(err) => {
                            ui.set_login_error(err.into());
                        }
                    }
                });
            });
        }
    });

    // Unlock an existing vault: decrypt with the password, pull the nsec, boot.
    ui.on_unlock({
        let weak = ui.as_weak();
        let boot = boot_backend.clone();
        move |password| {
            let Some(ui) = weak.upgrade() else { return };
            let password = password.to_string();
            ui.set_login_busy(true);
            // `Vault::open` re-derives the Argon2id key — worker thread, so
            // the unlock spinner actually spins while it grinds.
            let weak = weak.clone();
            let boot = boot.clone();
            std::thread::spawn(move || {
                type UnlockOutcome =
                    Result<(String, String, Arc<Mutex<Vault>>, Option<String>), String>;
                let result = (|| -> UnlockOutcome {
                    let v = Vault::open(&password).map_err(|e| match e {
                        vault::VaultError::WrongPassword => "Wrong password.".to_string(),
                        other => format!("{other}"),
                    })?;
                    let nsec = v.nsec().ok_or_else(|| {
                        "Vault has no key. Reset and re-enter your nsec.".to_string()
                    })?;
                    let keys =
                        Keys::parse(&nsec).map_err(|_| "Stored key is invalid.".to_string())?;
                    let npub = keys.public_key().to_bech32().map_err(|e| e.to_string())?;
                    // The account the user last had active — boot displays it
                    // instead of the primary when it still exists.
                    let active = v.get(vault::ACTIVE_ACCOUNT_KEY).map(|s| s.to_string());
                    Ok((npub, nsec, Arc::new(Mutex::new(v)), active))
                })();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    ui.set_login_busy(false);
                    match result {
                        Ok((npub, nsec, vault, active)) => {
                            ui.set_login_error(s(""));
                            ui.set_password_input(s(""));
                            ui.set_my_qr(qr_image(&format!("nostr:{npub}")));
                            ui.set_my_npub(npub.into());
                            ui.set_logged_in(true);
                            boot(nsec, vault, active);
                        }
                        Err(err) => {
                            ui.set_login_error(err.into());
                        }
                    }
                });
            });
        }
    });

    // "Reset & use another key" on the unlock screen. No password recovery exists,
    // so this deletes the vault and returns to first-run choose.
    ui.on_reset_vault({
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            if let Err(e) = vault::delete() {
                eprintln!("[login] vault reset failed: {e}");
            }
            // Queued sends were sealed under the old vault key — unreadable now.
            offline_queue::clear();
            ui.set_password_input(s(""));
            ui.set_password_confirm(s(""));
            ui.set_login_error(s(""));
            ui.set_login_mode(0);
        }
    });

    ui.on_generate_key_requested({
        let weak = ui.as_weak();
        let pending = pending_generated.clone();
        let pending_name = pending_profile_name.clone();
        move || {
            eprintln!("[login] generate_key_requested fired");
            let Some(ui) = weak.upgrade() else { return };
            let keys = Keys::generate();
            let nsec = match keys.secret_key().to_bech32() {
                Ok(v) => v,
                Err(e) => {
                    ui.set_login_error(format!("Failed to encode key: {e}").into());
                    return;
                }
            };
            let npub = match keys.public_key().to_bech32() {
                Ok(v) => v,
                Err(e) => {
                    ui.set_login_error(format!("Failed to encode key: {e}").into());
                    return;
                }
            };
            *pending.lock().unwrap() = Some(nsec.clone());
            let name = random_profile_name();
            *pending_name.lock().unwrap() = Some(name.clone());
            ui.set_generated_display_name(name.clone().into());
            if let Some(img) = local_animal_avatar_image(&npub, &name) {
                ui.set_generated_avatar(img);
                ui.set_generated_has_avatar(true);
            } else {
                ui.set_generated_has_avatar(false);
            }
            ui.set_generated_nsec(nsec.into());
            ui.set_generated_npub(npub.into());
            ui.set_login_error(s(""));
            ui.set_login_mode(2);
        }
    });

    ui.on_confirm_saved_key({
        let weak = ui.as_weak();
        let pending = pending_generated.clone();
        let pending_seed = pending_profile_seed.clone();
        let boot = boot_backend.clone();
        move |password, confirm| {
            eprintln!("[login] confirm_saved_key fired");
            let Some(ui) = weak.upgrade() else { return };
            let Some(nsec) = pending.lock().unwrap().clone() else {
                eprintln!("[login] no pending generated key");
                ui.set_login_error(s("No generated key to save. Try again."));
                ui.set_login_mode(0);
                return;
            };
            let password = password.to_string();
            ui.set_login_busy(true);
            // Vault creation runs the Argon2id KDF — off the UI thread.
            let weak = weak.clone();
            let boot = boot.clone();
            let pending = pending.clone();
            let pending_seed = pending_seed.clone();
            std::thread::spawn(move || {
                let result = (|| -> Result<(String, String, Arc<Mutex<Vault>>), String> {
                    validate_new_password(&password, confirm.as_str())?;
                    let keys = Keys::parse(&nsec).map_err(|e| format!("parse: {e}"))?;
                    let npub = keys
                        .public_key()
                        .to_bech32()
                        .map_err(|e| format!("npub encode: {e}"))?;
                    let id_hex = keys.public_key().to_hex();
                    let mut v =
                        Vault::create(&password).map_err(|e| format!("create vault: {e}"))?;
                    v.set(vault::NSEC_KEY, &nsec)
                        .map_err(|e| format!("seal nsec: {e}"))?;
                    Ok((npub, id_hex, Arc::new(Mutex::new(v))))
                })();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    ui.set_login_busy(false);
                    match result {
                        Ok((npub, id_hex, vault)) => {
                            eprintln!("[login] sealed nsec into vault, logging in as {npub}");
                            *pending.lock().unwrap() = None;
                            // Freshly generated key: have boot seed a random
                            // starter profile once it comes up.
                            *pending_seed.lock().unwrap() = Some(id_hex);
                            ui.set_login_error(s(""));
                            ui.set_my_qr(qr_image(&format!("nostr:{npub}")));
                            ui.set_my_npub(npub.into());
                            ui.set_generated_nsec(s(""));
                            ui.set_generated_npub(s(""));
                            ui.set_generated_display_name(s(""));
                            ui.set_generated_has_avatar(false);
                            ui.set_password_input(s(""));
                            ui.set_password_confirm(s(""));
                            ui.set_logged_in(true);
                            boot(nsec, vault, None);
                        }
                        Err(err) => {
                            eprintln!("[login] save failed: {err}");
                            ui.set_login_error(err.into());
                        }
                    }
                });
            });
        }
    });

    ui.on_copy_nsec({
        let weak = ui.as_weak();
        move |nsec| {
            let weak = weak.clone();
            copy_to_clipboard_async(nsec.to_string(), move |result| {
                if let Err(e) = result {
                    eprintln!("[clipboard] copy nsec failed: {e}");
                    return;
                }
                if let Some(ui) = weak.upgrade() {
                    ui.set_profile_status(s("nsec copied"));
                }
            });
        }
    });

    // ─── Debug pane ────────────────────────────────────────────────────
    // Settings persist the toggle across launches. The pane itself is gated
    // behind that toggle; when off, the sidebar entry doesn't even render.
    ui.set_debug_enabled(settings_cell.borrow().debug_enabled);

    ui.on_change_language_clicked({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_show_language_picker(true);
            }
        }
    });

    ui.on_locale_selected({
        let weak = ui.as_weak();
        let settings_cell = settings_cell.clone();
        move |code| {
            let locale = normalize_locale(code.as_str()).to_string();
            apply_locale(&locale);
            {
                let mut s = settings_cell.borrow_mut();
                s.locale = locale.clone();
                s.save();
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_locale(s(&locale));
                ui.set_locale_display(s(locale_display(&locale)));
                ui.set_show_language_picker(false);
                // Re-snapshot the now-localized error/status copy for worker threads.
                refresh_error_copy(&ui);
            }
        }
    });

    ui.on_theme_mode_selected({
        let weak = ui.as_weak();
        let settings_cell = settings_cell.clone();
        move |mode| {
            let mode = normalize_theme_mode(mode.as_str()).to_string();
            {
                let mut s = settings_cell.borrow_mut();
                s.theme = mode.clone();
                s.save();
            }
            if let Some(ui) = weak.upgrade() {
                apply_theme_mode(&ui, &mode);
            }
        }
    });

    ui.on_accent_selected({
        let weak = ui.as_weak();
        let settings_cell = settings_cell.clone();
        move |idx| {
            let color = match idx {
                1 => "ocean",
                2 => "berry",
                3 => "coral",
                4 => "lavender",
                _ => "mint",
            };
            {
                let mut s = settings_cell.borrow_mut();
                s.accent_color = color.to_string();
                s.save();
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_accent_color(idx);
            }
        }
    });

    ui.on_debug_toggled({
        let settings_cell = settings_cell.clone();
        move |on| {
            let mut s = settings_cell.borrow_mut();
            s.debug_enabled = on;
            s.save();
        }
    });

    ui.on_outgoing_on_right_toggled({
        let settings_cell = settings_cell.clone();
        move |on| {
            let mut s = settings_cell.borrow_mut();
            s.outgoing_on_right = on;
            s.save();
        }
    });

    ui.on_notifications_toggled({
        let settings_cell = settings_cell.clone();
        let notif = notif.clone();
        move |on| {
            notif
                .enabled
                .store(on, std::sync::atomic::Ordering::Relaxed);
            let mut s = settings_cell.borrow_mut();
            s.notifications_enabled = on;
            s.save();
        }
    });
    ui.on_notification_sound_toggled({
        let settings_cell = settings_cell.clone();
        let notif = notif.clone();
        move |on| {
            notif.sound.store(on, std::sync::atomic::Ordering::Relaxed);
            let mut s = settings_cell.borrow_mut();
            s.notification_sound = on;
            s.save();
        }
    });
    ui.on_notification_preview_toggled({
        let settings_cell = settings_cell.clone();
        let notif = notif.clone();
        move |on| {
            notif
                .preview
                .store(on, std::sync::atomic::Ordering::Relaxed);
            let mut s = settings_cell.borrow_mut();
            s.notification_preview = on;
            s.save();
        }
    });

    // Mute / unmute the currently-open chat (header bell). Flips the live
    // NotifState set + the persisted settings, and updates the header.
    ui.on_toggle_mute_chat({
        let weak = ui.as_weak();
        let group_ids = group_ids.clone();
        let settings_cell = settings_cell.clone();
        let notif = notif.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let idx = ui.get_active_chat();
            let group_hex = group_ids.lock().unwrap().get(idx as usize).cloned();
            let Some(group_hex) = group_hex else { return };
            let now_muted = !notif.is_muted(&group_hex);
            notif.set_muted(&group_hex, now_muted);
            {
                let mut s = settings_cell.borrow_mut();
                if now_muted {
                    s.muted_chats.insert(group_hex);
                } else {
                    s.muted_chats.remove(&group_hex);
                }
                s.save();
            }
            ui.set_active_chat_muted(now_muted);
        }
    });

    ui.on_time_format_selected({
        let weak = ui.as_weak();
        let settings_cell = settings_cell.clone();
        let backend_cell = backend_cell.clone();
        let pending_state = pending_state.clone();
        let group_ids = group_ids.clone();
        let archived_group_ids = archived_group_ids.clone();
        move |fmt| {
            let fmt = if fmt.as_str() == "12h" { "12h" } else { "24h" };
            {
                let mut st = settings_cell.borrow_mut();
                st.time_format = fmt.to_string();
                st.save();
                apply_stamp_formats(&st);
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_time_format(s(fmt));
                refresh_stamps_everywhere(
                    &ui,
                    &backend_cell,
                    &pending_state,
                    &group_ids,
                    &archived_group_ids,
                );
            }
        }
    });

    ui.on_date_format_selected({
        let weak = ui.as_weak();
        let settings_cell = settings_cell.clone();
        let backend_cell = backend_cell.clone();
        let pending_state = pending_state.clone();
        let group_ids = group_ids.clone();
        let archived_group_ids = archived_group_ids.clone();
        move |fmt| {
            let fmt = match fmt.as_str() {
                "dmy" => "dmy",
                "iso" => "iso",
                _ => "mdy",
            };
            {
                let mut st = settings_cell.borrow_mut();
                st.date_format = fmt.to_string();
                st.save();
                apply_stamp_formats(&st);
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_date_format(s(fmt));
                refresh_stamps_everywhere(
                    &ui,
                    &backend_cell,
                    &pending_state,
                    &group_ids,
                    &archived_group_ids,
                );
            }
        }
    });

    ui.on_debug_refresh_clicked({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        move || {
            // Liveness check only — the dump lands via the completion below.
            if weak.upgrade().is_none() {
                return;
            }
            // `debug_snapshot` does a `block_on` per group for MLS state —
            // collect it on a worker.
            let b = backend_cell.lock().unwrap().clone();
            let weak = weak.clone();
            std::thread::spawn(move || {
                let snap = b
                    .map(|b| b.debug_snapshot())
                    .unwrap_or_else(|| "(backend not booted)".to_string());
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    ui.set_debug_dump(snap.into());
                });
            });
        }
    });

    ui.on_debug_copy_clicked({
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let text = ui.get_debug_dump();
            if text.is_empty() {
                return;
            }
            copy_to_clipboard_async(text.to_string(), |result| {
                if let Err(e) = result {
                    eprintln!("[clipboard] copy debug dump failed: {e}");
                }
            });
        }
    });

    // ─── Security & privacy toggles ────────────────────────────────────
    ui.on_telemetry_toggled({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        move |on| {
            let Some(ui) = weak.upgrade() else { return };
            // The marmot settings store is a synchronous disk write — never
            // run it on the UI thread (or while holding the cell lock).
            let Some(b) = backend_cell.lock().ok().and_then(|g| g.as_ref().cloned()) else {
                ui.set_telemetry_enabled(!on);
                return;
            };
            let weak = ui.as_weak();
            std::thread::spawn(move || {
                let result = b.set_telemetry_enabled(on);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    if let Err(e) = result {
                        eprintln!("[settings] set telemetry failed: {e}");
                        ui.set_telemetry_enabled(!on);
                    }
                });
            });
        }
    });

    ui.on_audit_toggled({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        move |on| {
            let Some(ui) = weak.upgrade() else { return };
            let Some(b) = backend_cell.lock().ok().and_then(|g| g.as_ref().cloned()) else {
                ui.set_audit_enabled(!on);
                return;
            };
            // Persist + hot-swap the recorder on running sessions (no restart).
            // Applying the switch awaits each account worker's FIFO queue, which
            // a misbehaving relay can hold for ~35s — never block here.
            let weak = ui.as_weak();
            let fut = b.set_audit_logs_enabled(on);
            b.tokio_handle().spawn(async move {
                let result = fut.await;
                let files = b.audit_log_files().unwrap_or_default();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    match result {
                        Ok(()) => ui.set_audit_status(
                            if on {
                                "Audit logging enabled — recording now; \
                                 logs upload automatically."
                            } else {
                                "Audit logging disabled. Existing files stay \
                                 until you delete them."
                            }
                            .into(),
                        ),
                        Err(e) => {
                            eprintln!("[settings] set audit logs failed: {e:#}");
                            ui.set_audit_enabled(!on);
                            ui.set_audit_status("Couldn't change audit logging.".into());
                        }
                    }
                    push_audit_files(&ui, files);
                });
            });
        }
    });

    ui.on_audit_refresh_files({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(b) = backend_cell.lock().ok().and_then(|g| g.as_ref().cloned()) else {
                return;
            };
            refresh_audit_files(&ui, &b);
        }
    });

    ui.on_audit_delete_file({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        move |path| {
            let Some(ui) = weak.upgrade() else { return };
            let Some(b) = backend_cell.lock().ok().and_then(|g| g.as_ref().cloned()) else {
                return;
            };
            let weak = ui.as_weak();
            let fut = b.delete_audit_log_file(path.to_string());
            b.tokio_handle().spawn(async move {
                let result = fut.await;
                let files = b.audit_log_files().unwrap_or_default();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    match result {
                        // `true` = the live recorder owned that file and
                        // rotated in place rather than going dark.
                        Ok(true) => ui.set_audit_status(
                            "Audit log deleted — recording continues in a fresh file.".into(),
                        ),
                        Ok(false) => ui.set_audit_status("Audit log deleted.".into()),
                        Err(e) => {
                            eprintln!("[settings] delete audit log failed: {e:#}");
                            ui.set_audit_status("Couldn't delete audit log.".into());
                        }
                    }
                    push_audit_files(&ui, files);
                });
            });
        }
    });

    // ─── Network & relays pane ─────────────────────────────────────────
    // The on-disk list (`backend::load_relays`) is the source of truth and
    // what we mutate from the UI. `backend.booted_relays()` is what the
    // running runtime was started with — when they diverge the pane shows a
    // "restart to apply" banner. MarmotApp has no `set_relays` API; pushing
    // the new list into the live runtime would require a much larger refactor,
    // so for now the user restarts to pick up changes.
    //
    // `network-status` is the transient line under the list — error text on
    // bad input or save failures, brief confirmation on success.

    // Initial population — the on-disk list always exists (possibly empty)
    // even before the backend boots; booted-relays + health stay empty until
    // backend ready, then we re-push.
    {
        // Routes through push_network_relays so suggested-relay chips are seeded too.
        let initial = backend::load_relays();
        push_network_relays(&ui, &initial);
        ui.set_network_booted_relays(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
        ui.set_network_connected(0);
        ui.set_network_total(0);
        ui.set_network_status(s(""));
    }

    // On the first-run get-started screen the backend is booted *before* the
    // user has configured any relay (`load_relays()` is empty at boot), and
    // MarmotApp exposes no live `set_relays`. So a relay added there would only
    // ever land on disk — never on the running transport — which is why it
    // "does nothing" until the next restart. To make the welcome flow actually
    // work, re-boot the runtime against the new on-disk list whenever it
    // changes while we're still in the no-chats first-run state. Once a chat
    // exists the Settings → Network pane is the only entry point, and it keeps
    // its intentional "restart to apply" banner rather than yanking a live
    // session out from under the user.
    let reboot_relays_first_run: Rc<dyn Fn()> = {
        let weak = ui.as_weak();
        let boot = boot_backend.clone();
        let vault_cell = vault_cell.clone();
        Rc::new(move || {
            let Some(ui) = weak.upgrade() else { return };
            // Only when a previous boot has settled (avoid racing a boot in
            // flight) and we're still on the first-run get-started screen.
            if !ui.get_backend_ready() {
                return;
            }
            if ui.get_chats().row_count() > 0 {
                return;
            }
            let Some(vault) = vault_cell.lock().unwrap().clone() else {
                return;
            };
            let Some(nsec) = vault.lock().unwrap().nsec() else {
                return;
            };
            // `boot` re-reads `load_relays()` (already saved below), spawns a
            // fresh runtime, and on success replaces backend_cell + re-pushes
            // the live connection counts via refresh_network_post_boot.
            boot(nsec, vault, None);
        })
    };

    ui.on_network_add_relay({
        let weak = ui.as_weak();
        let reboot = reboot_relays_first_run.clone();
        move |raw| {
            let Some(ui) = weak.upgrade() else { return };
            let trimmed = raw.trim().to_string();
            if let Err(msg) = validate_relay_url(&trimmed) {
                ui.set_network_status(msg.into());
                return;
            }
            let mut list: Vec<String> = vec_string_from_model(&ui.get_network_relays());
            if list.iter().any(|u| u.eq_ignore_ascii_case(&trimmed)) {
                ui.set_network_status(error_copy().relay_already_listed.into());
                return;
            }
            list.push(trimmed);
            if let Err(e) = backend::save_relays(&list) {
                eprintln!("[network] save relays failed: {e}");
                ui.set_network_status(error_copy().save_relays_failed.into());
                return;
            }
            push_network_relays(&ui, &list);
            ui.set_network_status(error_copy().relay_added.into());
            // First-run: connect the freshly-added relay live (no-op otherwise).
            reboot();
        }
    });

    ui.on_network_remove_relay({
        let weak = ui.as_weak();
        let reboot = reboot_relays_first_run.clone();
        move |url| {
            let Some(ui) = weak.upgrade() else { return };
            let mut list: Vec<String> = vec_string_from_model(&ui.get_network_relays());
            let before = list.len();
            list.retain(|u| u != url.as_str());
            if list.len() == before {
                return;
            }
            if let Err(e) = backend::save_relays(&list) {
                eprintln!("[network] save relays failed: {e}");
                ui.set_network_status(error_copy().save_relays_failed.into());
                return;
            }
            push_network_relays(&ui, &list);
            ui.set_network_status(error_copy().relay_removed.into());
            // First-run: re-boot so the live transport drops the removed relay.
            reboot();
        }
    });

    ui.on_network_refresh_health({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let weak = weak.clone();
            let backend_cell = backend_cell.clone();
            std::thread::spawn(move || {
                // Clone the handle, drop the lock, then poll — the UI thread
                // must never find this mutex held across a relay query.
                let b = backend_cell.lock().unwrap().clone();
                let snapshot = b.map(|b| b.relay_health());
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    match snapshot {
                        Some((connected, total)) => {
                            ui.set_network_connected(connected as i32);
                            ui.set_network_total(total as i32);
                            // We just polled the relay pool — that's a real sync.
                            ui.set_sync_secs(0);
                        }
                        None => ui.set_network_status(error_copy().not_connected.into()),
                    }
                });
            });
            ui.set_network_status(s(""));
        }
    });

    ui.on_network_republish_relay_list({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_network_status(error_copy().republishing.into());
            let weak = weak.clone();
            let backend_cell = backend_cell.clone();
            std::thread::spawn(move || {
                // Same handle-clone dance: never hold the cell lock across
                // the relay publish.
                let b = backend_cell.lock().unwrap().clone();
                let result = match b {
                    None => Err(error_copy().not_connected),
                    Some(b) => b
                        .republish_relay_lists()
                        .map_err(|e| friendly_error("republish", &e)),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    match result {
                        Ok(n) => ui.set_network_status(
                            format!("Republished to {n} relay{}.", if n == 1 { "" } else { "s" })
                                .into(),
                        ),
                        Err(e) => ui.set_network_status(e.into()),
                    }
                });
            });
        }
    });

    // ─── Keys page: KP publish / rotate / refresh ──────────────────────
    // All three call into the marmot runtime, which blocks on its tokio
    // executor — so we hop onto a worker thread first, then back to the
    // Slint event loop with the results. UI sets `kp-busy` for the
    // round-trip so buttons can disable themselves visually.

    let kp_run = {
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        // op_kind: "publish" | "rotate" | "refresh"
        Rc::new(move |op_kind: &'static str| {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_kp_busy(true);
            ui.set_kp_status(format!("{op_kind}…").into());
            let weak = weak.clone();
            // Clone the backend handle and drop the lock before the relay
            // round-trip — other callbacks keep locking this cell freely.
            let b = backend_cell.lock().unwrap().clone();
            std::thread::spawn(move || {
                let result: Result<String, String> = {
                    match b.as_deref() {
                        None => Err(error_copy().not_connected),
                        Some(b) => match op_kind {
                            // NOTE: the SDK returns the key-package size in bytes,
                            // not a relay-ack count — so we don't surface the number
                            // (it was being shown as a nonsensical "N relay acks").
                            "publish" => b
                                .publish_key_package()
                                .map(|_| "published · your key package is live".to_string())
                                .map_err(|e| friendly_error("kp_publish", &e)),
                            "rotate" => b
                                .rotate_key_package()
                                .map(|_| "rotated · published a fresh key package".to_string())
                                .map_err(|e| friendly_error("kp_rotate", &e)),
                            "refresh" => b
                                .key_packages_fetch()
                                .map(|recs| {
                                    format!(
                                        "fetched · {} record{}",
                                        recs.len(),
                                        if recs.len() == 1 { "" } else { "s" }
                                    )
                                })
                                .map_err(|e| friendly_error("kp_refresh", &e)),
                            _ => Err("Something went wrong. Please try again.".to_string()),
                        },
                    }
                };
                // The post-op snapshot for "refresh" hits relays too — pull
                // the rows here on the worker, never in the event-loop
                // completion (that closure runs on the UI thread).
                let rows: Option<Vec<KeyPackageInfo>> = b.as_deref().and_then(|b| {
                    if op_kind == "refresh" {
                        b.key_packages_fetch()
                            .ok()
                            .map(|recs| recs.iter().map(kp_to_ui).collect())
                    } else {
                        None
                    }
                });
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    ui.set_kp_busy(false);
                    match result {
                        Ok(status) => ui.set_kp_status(status.into()),
                        Err(e) => ui.set_kp_status(e.into()),
                    }
                    // Refresh from local state regardless of op outcome; for
                    // "refresh" we additionally surface the relay snapshot.
                    if let Some(b) = b.as_ref() {
                        if let Some(rows) = rows {
                            ui.set_key_packages(ModelRc::new(VecModel::from(rows)));
                        } else {
                            refresh_kp_local_async(&ui, b);
                        }
                    }
                });
            });
        })
    };

    ui.on_kp_publish_clicked({
        let kp_run = kp_run.clone();
        move || kp_run("publish")
    });
    ui.on_kp_rotate_clicked({
        let kp_run = kp_run.clone();
        move || kp_run("rotate")
    });
    ui.on_kp_refresh_clicked({
        let kp_run = kp_run.clone();
        move || kp_run("refresh")
    });

    ui.on_copy_to_clipboard({
        let weak = ui.as_weak();
        move |text| {
            eprintln!(
                "[ui] copy-to-clipboard fired, text empty={}",
                text.is_empty()
            );
            let Some(ui) = weak.upgrade() else { return };
            if text.is_empty() {
                ui.set_profile_status(s("nothing to copy (npub empty)"));
                return;
            }
            let weak = weak.clone();
            copy_to_clipboard_async(text.to_string(), move |result| {
                let Some(ui) = weak.upgrade() else { return };
                match result {
                    Ok(()) => ui.set_profile_status(s("npub copied")),
                    Err(e) => {
                        eprintln!("[clipboard] copy failed: {e}");
                        ui.set_profile_status(format!("clipboard error: {e}").into());
                    }
                }
            });
        }
    });

    // After any selection mutation, refresh the breadcrumb so the title bar matches state.
    // Captures only the weak handle, so clones are `Send` and can ride
    // through worker threads into completion closures.
    let refresh_breadcrumb = {
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            refresh_breadcrumb_now(&ui);
        }
    };
    refresh_breadcrumb();

    // Recompute the Storage pane's media-cache size off the UI thread (disk
    // walk) and push the formatted label back. Cheap, but IO — never inline.
    let refresh_storage_size = {
        let weak = ui.as_weak();
        move || {
            let weak = weak.clone();
            std::thread::spawn(move || {
                let label = human_bytes(media_cache::size_bytes());
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_storage_cache_size(label.into());
                    }
                });
            });
        }
    };
    refresh_storage_size();
    // Static for the session — the data dir doesn't move while we're running.
    ui.set_storage_vault_dir(vault::vault_dir().display().to_string().into());

    // Reveal the folder holding vault.db in the platform file manager. Reuses the
    // same xdg-open/open handler as external links — a directory path is fine.
    ui.on_storage_open_vault_folder(move || {
        open_external(&vault::vault_dir().display().to_string());
    });

    // ─── Whole-folder backup & restore ─────────────────────────────────
    // A backup is the entire data dir packed into one file, sealed with the
    // vault password (see backup.rs). Open the create-backup modal.
    ui.on_storage_create_backup({
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_create_backup_password(s(""));
            ui.set_create_backup_status(s(""));
            ui.set_create_backup_busy(false);
            ui.set_show_create_backup(true);
        }
    });

    ui.on_create_backup_dismissed({
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_show_create_backup(false);
            ui.set_create_backup_password(s(""));
            ui.set_create_backup_status(s(""));
        }
    });

    // Confirm vault password → native save dialog → write the encrypted backup.
    // The picker is sync rfd on a plain thread (no backend needed, and never on
    // the UI thread).
    ui.on_create_backup_submit({
        let weak = ui.as_weak();
        move |password| {
            let Some(ui) = weak.upgrade() else { return };
            let password = password.to_string();
            if password.is_empty() {
                ui.set_create_backup_status(s("Enter your vault password."));
                return;
            }
            ui.set_create_backup_busy(true);
            ui.set_create_backup_status(s(""));
            let weak = weak.clone();
            std::thread::spawn(move || {
                let dest = rfd::FileDialog::new()
                    .set_title("Save backup")
                    .set_file_name(backup::DEFAULT_FILENAME)
                    .save_file();
                let Some(dest) = dest else {
                    // Cancelled — drop the busy state, leave the modal open.
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.set_create_backup_busy(false);
                        }
                    });
                    return;
                };
                let result = backup::create(&dest, &password);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    ui.set_create_backup_busy(false);
                    match result {
                        Ok(()) => {
                            ui.set_show_create_backup(false);
                            ui.set_create_backup_password(s(""));
                        }
                        Err(backup::BackupError::WrongPassword) => {
                            ui.set_create_backup_status(s("Wrong vault password."));
                        }
                        Err(e) => {
                            ui.set_create_backup_status(format!("Backup failed: {e}").into());
                        }
                    }
                });
            });
        }
    });

    // Open the import-backup modal. On a fresh install (no vault) it restores the
    // whole folder; otherwise it merges accounts — the modal copy follows suit.
    ui.on_storage_import_backup({
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_import_backup_path(s(""));
            ui.set_import_backup_file(s(""));
            ui.set_import_backup_password(s(""));
            ui.set_import_backup_status(s(""));
            ui.set_import_backup_busy(false);
            ui.set_import_backup_restore_mode(!vault::exists());
            ui.set_show_import_backup(true);
        }
    });

    ui.on_import_backup_dismissed({
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_show_import_backup(false);
            ui.set_import_backup_path(s(""));
            ui.set_import_backup_password(s(""));
            ui.set_import_backup_status(s(""));
        }
    });

    // Native file picker for the backup file. Sync rfd on a plain thread so it
    // works before the backend exists (first-run restore) and never blocks the UI
    // thread. The chosen path round-trips through a Slint property (Send-safe).
    ui.on_import_backup_pick_file({
        let weak = ui.as_weak();
        move || {
            let weak = weak.clone();
            std::thread::spawn(move || {
                let Some(picked) = rfd::FileDialog::new()
                    .set_title("Import backup")
                    .pick_file()
                else {
                    return;
                };
                let name = picked
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| picked.display().to_string());
                let path = picked.display().to_string();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    ui.set_import_backup_path(path.into());
                    ui.set_import_backup_file(name.into());
                    ui.set_import_backup_status(s(""));
                });
            });
        }
    });

    // Submit: decrypt the backup, then either restore the whole folder (fresh
    // install) or merge its accounts (running install). The branch is decided by
    // whether a vault already exists.
    ui.on_import_backup_submit({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let vault_cell = vault_cell.clone();
        move |password| {
            let Some(ui) = weak.upgrade() else { return };
            let path = ui.get_import_backup_path().to_string();
            if path.is_empty() {
                ui.set_import_backup_status(s("Choose a backup file first."));
                return;
            }
            if password.is_empty() {
                ui.set_import_backup_status(s("Enter the backup password."));
                return;
            }
            let path = std::path::PathBuf::from(path);
            let password = password.to_string();
            // Act on the mode the modal is actually showing (Restore vs Import),
            // not a freshly-recomputed predicate — the displayed copy and the
            // backend action stay in lockstep. The property was set from
            // `!vault::exists()` when the modal opened.
            let restoring = ui.get_import_backup_restore_mode();
            // `restore_into_home` overwrites the data dir, so re-check vault
            // presence now (not just at open time): a full restore must never
            // clobber an identity that came to exist while the modal was open.
            if restoring && vault::exists() {
                ui.set_import_backup_status(s(
                    "Full restore is only available before unlocking an existing vault.",
                ));
                return;
            }
            ui.set_import_backup_busy(true);
            ui.set_import_backup_status(s(""));
            let weak = weak.clone();
            let backend_cell = backend_cell.clone();
            let vault_cell = vault_cell.clone();
            // Argon2id derive + archive IO — off the UI thread.
            std::thread::spawn(move || {
                if restoring {
                    // Fresh install: extract the whole folder, then unlock the
                    // restored vault with the same password to boot straight in.
                    let result = backup::restore_into_home(&path, &password);
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = weak.upgrade() else { return };
                        match result {
                            Ok(()) => {
                                ui.set_import_backup_busy(false);
                                ui.set_show_import_backup(false);
                                ui.set_import_backup_password(s(""));
                                // The restored vault.db unlocks with this very
                                // password — reuse the unlock path to boot.
                                ui.invoke_unlock(password.into());
                            }
                            Err(e) => {
                                ui.set_import_backup_busy(false);
                                ui.set_import_backup_status(import_backup_error(&e).into());
                            }
                        }
                    });
                } else {
                    // Running install: pull keys from the backup's vault.db and
                    // re-login the missing accounts.
                    let result = backup::merge_nsecs(&path, &password);
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = weak.upgrade() else { return };
                        let nsecs = match result {
                            Ok(n) => n,
                            Err(e) => {
                                ui.set_import_backup_busy(false);
                                ui.set_import_backup_status(import_backup_error(&e).into());
                                return;
                            }
                        };
                        let Some(backend) = backend_cell.lock().unwrap().clone() else {
                            ui.set_import_backup_busy(false);
                            ui.set_import_backup_status(s("Backend isn't ready yet."));
                            return;
                        };
                        merge_imported_accounts(&ui, &backend, &vault_cell, nsecs);
                    });
                }
            });
        }
    });

    let go_to_page = {
        let weak = ui.as_weak();
        let refresh = refresh_breadcrumb.clone();
        let refresh_storage = refresh_storage_size.clone();
        move |page: Page| {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_active_page(page as i32);
            refresh();
            // Settings can land on the Storage tab — make sure the size is fresh.
            if matches!(page, Page::Settings) {
                refresh_storage();
            }
        }
    };

    ui.on_storage_clear_cache({
        let weak = ui.as_weak();
        let refresh_storage = refresh_storage_size.clone();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_storage_clearing(true);
            }
            let weak = weak.clone();
            let refresh_storage = refresh_storage.clone();
            std::thread::spawn(move || {
                media_cache::clear();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_storage_clearing(false);
                    }
                });
                // Repopulate the (now ~0) size label.
                refresh_storage();
            });
        }
    });

    ui.on_nav_requested({
        let go = go_to_page.clone();
        move |idx| {
            let page = match idx {
                0 => Page::Chats,
                1 => Page::Contacts,
                2 => Page::Archived,
                3 => Page::Keys,
                4 => Page::Settings,
                _ => Page::Chats,
            };
            go(page);
        }
    });
    ui.on_profile_requested({
        let go = go_to_page.clone();
        move || go(Page::Profile)
    });
    ui.on_new_chat_requested({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_show_new_chat(true);
            }
        }
    });
    ui.on_modal_dismissed({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_show_new_chat(false);
                ui.set_new_chat_name(s(""));
                ui.set_new_chat_members(s(""));
                ui.set_new_chat_status(s(""));
                ui.set_new_chat_busy(false);
            }
        }
    });
    ui.on_start_chat({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        move |name, members_text| {
            let Some(ui) = weak.upgrade() else { return };
            let name = name.to_string();
            let members = parse_member_list(&members_text);
            if members.is_empty() {
                ui.set_new_chat_status(s("Add at least one npub."));
                return;
            }
            let Some(b) = backend_cell.lock().unwrap().clone() else {
                ui.set_new_chat_status(s("Backend not ready."));
                return;
            };
            // Skip the creator's own npub if it leaked into the input —
            // marmot rejects self-invites.
            let me_npub = npub_for_account_id(&b.account().account_id_hex).ok();
            let members: Vec<String> = members
                .into_iter()
                .filter(|m| {
                    me_npub
                        .as_deref()
                        .map(|n| !m.eq_ignore_ascii_case(n))
                        .unwrap_or(true)
                })
                .collect();
            if members.is_empty() {
                ui.set_new_chat_status(s("Can't start a chat with only yourself."));
                return;
            }
            ui.set_new_chat_busy(true);
            ui.set_new_chat_status(s(""));
            let group_name = if name.trim().is_empty() && members.len() == 1 {
                String::new()
            } else if name.trim().is_empty() {
                "New group".to_string()
            } else {
                name.trim().to_string()
            };
            // `create_group` fetches key packages and publishes welcomes —
            // relay round-trips, so a worker does them while the busy state
            // paints.
            let weak = weak.clone();
            let group_ids = group_ids.clone();
            std::thread::spawn(move || {
                let result = b.create_group(&group_name, &members);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    ui.set_new_chat_busy(false);
                    match result {
                        Ok(group_id) => {
                            let group_hex = hex::encode(group_id.as_slice());
                            // Select the freshly-created chat in the continuation,
                            // once the refreshed snapshot is applied. The runtime
                            // appends it to the visible-chats projection
                            // synchronously after create_group resolves, so it
                            // should be present.
                            refresh_chats_async(&ui, &b, &group_ids, move |ui, _b, snap| {
                                let pos = snap
                                    .records
                                    .iter()
                                    .position(|r| r.group_id_hex.eq_ignore_ascii_case(&group_hex));
                                if let Some(pos) = pos {
                                    ui.set_active_chat(pos as i32);
                                    ui.invoke_chat_selected(pos as i32);
                                }
                            });
                            ui.set_new_chat_name(s(""));
                            ui.set_new_chat_members(s(""));
                            ui.set_new_chat_status(s(""));
                            ui.set_show_new_chat(false);
                        }
                        Err(e) => {
                            eprintln!("[create-group] {e:#}");
                            ui.set_new_chat_status(friendly_error("create chat", &e).into());
                        }
                    }
                });
            });
        }
    });
    ui.on_add_contact_requested({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_show_add_contact(true);
            }
        }
    });
    ui.on_add_contact_dismissed({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_show_add_contact(false);
                ui.set_add_contact_input(s(""));
                ui.set_add_contact_status(s(""));
                ui.set_add_contact_busy(false);
            }
        }
    });
    ui.on_add_contact({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        move |input| {
            let Some(ui) = weak.upgrade() else { return };
            let input = input.trim().to_string();
            if input.is_empty() {
                ui.set_add_contact_status(s("Paste an npub or hex pubkey."));
                return;
            }
            let Some(b) = backend_cell.lock().unwrap().clone() else {
                ui.set_add_contact_status(s("Backend not ready."));
                return;
            };
            ui.set_add_contact_busy(true);
            ui.set_add_contact_status(s(""));
            // `add_contact` publishes the follow list and runs a broad
            // directory refresh across relays — worker thread.
            let weak = weak.clone();
            std::thread::spawn(move || {
                let result = b.add_contact(&input);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    ui.set_add_contact_busy(false);
                    match result {
                        Ok(account_id_hex) => {
                            // Select the freshly-added row (in the continuation,
                            // once the refreshed model is applied) so the detail
                            // pane shows it.
                            refresh_contacts_async(&ui, &b, move |ui| {
                                if let Ok(npub) = npub_for_account_id(&account_id_hex)
                                    && let Some(pos) = ui.get_contacts().iter().position(|c| {
                                        c.npub_full.as_str().eq_ignore_ascii_case(&npub)
                                    })
                                {
                                    ui.set_active_contact(pos as i32);
                                }
                            });
                            ui.set_add_contact_input(s(""));
                            ui.set_add_contact_status(s(""));
                            ui.set_show_add_contact(false);
                            refresh_breadcrumb_now(&ui);
                        }
                        Err(e) => {
                            eprintln!("[add-contact] {e:#}");
                            ui.set_add_contact_status(friendly_error("add contact", &e).into());
                        }
                    }
                });
            });
        }
    });
    // "Add contact" from the peer-profile modal — same flow as the add-contact
    // modal, but feedback stays inside the profile modal (badge flip / status).
    ui.on_peer_profile_add_contact({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let npub = ui.get_peer_profile_npub().to_string();
            if npub.is_empty() {
                return;
            }
            let Some(b) = backend_cell.lock().unwrap().clone() else {
                return;
            };
            ui.set_peer_profile_adding(true);
            ui.set_peer_profile_status(s(""));
            let weak = weak.clone();
            std::thread::spawn(move || {
                let result = b.add_contact(&npub);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    ui.set_peer_profile_adding(false);
                    match result {
                        Ok(_) => {
                            refresh_contacts_async(&ui, &b, |_| {});
                            ui.set_peer_profile_is_contact(true);
                            refresh_breadcrumb_now(&ui);
                        }
                        Err(e) => {
                            eprintln!("[profile-add-contact] {e:#}");
                            ui.set_peer_profile_status(friendly_error("add contact", &e).into());
                        }
                    }
                });
            });
        }
    });
    ui.on_contact_nickname_requested({
        let weak = ui.as_weak();
        let contacts = contacts.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(row) = contacts.row_data(ui.get_active_contact() as usize) else {
                return;
            };
            ui.set_nickname_input(row.nickname.clone());
            ui.set_nickname_contact_name(row.real_name.clone());
            ui.set_show_nickname_modal(true);
        }
    });
    ui.on_nickname_modal_dismissed({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_show_nickname_modal(false);
                ui.set_nickname_input(s(""));
            }
        }
    });
    ui.on_set_contact_nickname({
        let weak = ui.as_weak();
        let contacts = contacts.clone();
        let settings_cell = settings_cell.clone();
        let refresh = refresh_breadcrumb.clone();
        move |nick| {
            let Some(ui) = weak.upgrade() else { return };
            let idx = ui.get_active_contact() as usize;
            let Some(mut row) = contacts.row_data(idx) else {
                return;
            };
            let nick = nick.trim().to_string();
            {
                let mut st = settings_cell.borrow_mut();
                if nick.is_empty() {
                    st.nicknames.remove(row.account_id.as_str());
                } else {
                    st.nicknames
                        .insert(row.account_id.to_string(), nick.clone());
                }
                st.save();
            }
            // Patch the one row in place — no relay round-trip involved.
            row.name = if nick.is_empty() {
                row.real_name.clone()
            } else {
                nick.clone().into()
            };
            row.nickname = nick.into();
            contacts.set_row_data(idx, row);
            ui.set_show_nickname_modal(false);
            ui.set_nickname_input(s(""));
            refresh();
        }
    });
    ui.on_add_member({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        move |npub| {
            let Some(ui) = weak.upgrade() else { return };
            let npub = npub.trim().to_string();
            if npub.is_empty() {
                return;
            }
            let idx = ui.get_active_chat() as usize;
            let Some(group_hex) = group_ids.lock().unwrap().get(idx).cloned() else {
                return;
            };
            let Some(b) = backend_cell.lock().unwrap().clone() else {
                ui.set_add_member_status(s("Backend not ready."));
                return;
            };
            ui.set_add_member_busy(true);
            ui.set_add_member_status(s(""));
            // Inviting publishes an MLS commit + welcome to relays — worker.
            let weak = weak.clone();
            std::thread::spawn(move || {
                let result = b.invite_members(&group_hex, std::slice::from_ref(&npub));
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    ui.set_add_member_busy(false);
                    match result {
                        Ok(_) => {
                            push_group_members_to_ui_async(&ui, &b, &group_hex);
                            ui.set_add_member_draft(s(""));
                            ui.set_add_member_status(s("Invited."));
                        }
                        Err(e) => {
                            eprintln!("[invite] {e:#}");
                            ui.set_add_member_status(friendly_error("add member", &e).into());
                        }
                    }
                });
            });
        }
    });
    ui.on_promote_admin({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        move |member_id| {
            let Some(ui) = weak.upgrade() else { return };
            let member_id = member_id.trim().to_string();
            if member_id.is_empty() {
                return;
            }
            let idx = ui.get_active_chat() as usize;
            let Some(group_hex) = group_ids.lock().unwrap().get(idx).cloned() else {
                return;
            };
            ui.set_group_settings_status(s(""));
            let Some(b) = backend_cell.lock().unwrap().clone() else {
                ui.set_group_settings_status(s("Backend not ready."));
                return;
            };
            // Admin changes publish an MLS commit to relays — worker.
            let weak = weak.clone();
            std::thread::spawn(move || {
                let result = b.promote_admin(&group_hex, &member_id);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    match result {
                        Ok(_) => {
                            push_group_members_to_ui_async(&ui, &b, &group_hex);
                            ui.set_group_settings_status(s("Admin added."));
                        }
                        Err(e) => {
                            eprintln!("[promote] {e:#}");
                            ui.set_group_settings_status(
                                friendly_error("group settings", &e).into(),
                            );
                        }
                    }
                });
            });
        }
    });
    ui.on_demote_admin({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        move |member_id| {
            let Some(ui) = weak.upgrade() else { return };
            let member_id = member_id.trim().to_string();
            if member_id.is_empty() {
                return;
            }
            let idx = ui.get_active_chat() as usize;
            let Some(group_hex) = group_ids.lock().unwrap().get(idx).cloned() else {
                return;
            };
            ui.set_group_settings_status(s(""));
            let Some(b) = backend_cell.lock().unwrap().clone() else {
                ui.set_group_settings_status(s("Backend not ready."));
                return;
            };
            let weak = weak.clone();
            std::thread::spawn(move || {
                let result = b.demote_admin(&group_hex, &member_id);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    match result {
                        Ok(_) => {
                            push_group_members_to_ui_async(&ui, &b, &group_hex);
                            ui.set_group_settings_status(s("Admin removed."));
                        }
                        Err(e) => {
                            eprintln!("[demote] {e:#}");
                            ui.set_group_settings_status(
                                friendly_error("group settings", &e).into(),
                            );
                        }
                    }
                });
            });
        }
    });
    ui.on_self_demote_admin({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let idx = ui.get_active_chat() as usize;
            let Some(group_hex) = group_ids.lock().unwrap().get(idx).cloned() else {
                return;
            };
            ui.set_group_settings_status(s(""));
            let Some(b) = backend_cell.lock().unwrap().clone() else {
                ui.set_group_settings_status(s("Backend not ready."));
                return;
            };
            let weak = weak.clone();
            std::thread::spawn(move || {
                let result = b.self_demote_admin(&group_hex);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    match result {
                        Ok(_) => {
                            push_group_members_to_ui_async(&ui, &b, &group_hex);
                            ui.set_group_settings_status(s("You stepped down."));
                        }
                        Err(e) => {
                            eprintln!("[self-demote] {e:#}");
                            ui.set_group_settings_status(
                                friendly_error("group settings", &e).into(),
                            );
                        }
                    }
                });
            });
        }
    });
    ui.on_rename_group({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        move |name| {
            let Some(ui) = weak.upgrade() else { return };
            let name = name.trim().to_string();
            if name.is_empty() {
                ui.set_group_settings_status(s("Name can't be empty."));
                return;
            }
            let idx = ui.get_active_chat() as usize;
            let Some(group_hex) = group_ids.lock().unwrap().get(idx).cloned() else {
                return;
            };
            let Some(b) = backend_cell.lock().unwrap().clone() else {
                ui.set_group_settings_status(s("Backend not ready."));
                return;
            };
            ui.set_group_rename_busy(true);
            ui.set_group_settings_status(s(""));
            // Renaming publishes an MLS commit to relays — worker.
            let weak = weak.clone();
            let group_ids = group_ids.clone();
            std::thread::spawn(move || {
                let result = b.rename_group(&group_hex, &name);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    ui.set_group_rename_busy(false);
                    match result {
                        Ok(_) => {
                            refresh_chats_async(&ui, &b, &group_ids, |_, _, _| {});
                            push_group_members_to_ui_async(&ui, &b, &group_hex);
                            ui.set_group_settings_status(s("Renamed."));
                        }
                        Err(e) => {
                            eprintln!("[rename] {e:#}");
                            ui.set_group_settings_status(
                                friendly_error("group settings", &e).into(),
                            );
                        }
                    }
                });
            });
        }
    });
    ui.on_clear_group_image({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_group_image_busy() {
                return;
            }
            let idx = ui.get_active_chat() as usize;
            let Some(group_hex) = group_ids.lock().unwrap().get(idx).cloned() else {
                return;
            };
            ui.set_group_image_busy(true);
            ui.set_group_settings_status(s("removing image…"));
            let weak_done = ui.as_weak();
            let backend_cell_done = backend_cell.clone();
            let group_ids = group_ids.clone();
            let group_hex_done = group_hex.clone();
            let guard = backend_cell.lock().unwrap();
            let Some(b) = guard.as_ref() else {
                ui.set_group_image_busy(false);
                ui.set_group_settings_status(s("Backend not ready."));
                return;
            };
            b.set_group_image_async(&group_hex, Vec::new(), String::new(), move |result| {
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak_done.upgrade() else {
                        return;
                    };
                    ui.set_group_image_busy(false);
                    match result {
                        Ok(_) => {
                            ui.set_group_settings_status(s("image removed"));
                            if let Some(b) = backend_cell_done.lock().unwrap().as_ref() {
                                refresh_chats_async(&ui, b, &group_ids, |_, _, _| {});
                                push_group_members_to_ui_async(&ui, b, &group_hex_done);
                            }
                        }
                        Err(e) => {
                            eprintln!("[group-image] clear failed: {e:#}");
                            ui.set_group_settings_status(friendly_error("group image", &e).into());
                        }
                    }
                });
            });
        }
    });
    ui.on_change_group_image({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_group_image_busy() {
                return;
            }
            let idx = ui.get_active_chat() as usize;
            let Some(group_hex) = group_ids.lock().unwrap().get(idx).cloned() else {
                return;
            };
            let tokio_handle = {
                let guard = backend_cell.lock().unwrap();
                match guard.as_ref() {
                    Some(b) => b.tokio_handle(),
                    None => {
                        ui.set_group_settings_status(s("backend not ready"));
                        return;
                    }
                }
            };
            ui.set_group_image_busy(true);
            ui.set_group_settings_status(s("choosing image…"));
            let weak = ui.as_weak();
            let backend_cell = backend_cell.clone();
            let group_ids = group_ids.clone();
            tokio_handle.spawn(async move {
                let chosen = tokio::task::spawn_blocking(|| {
                    rfd::FileDialog::new()
                        .set_title("Choose a group image")
                        .add_filter("Images", &["png", "jpg", "jpeg", "gif", "webp"])
                        .pick_file()
                })
                .await
                .ok()
                .flatten();

                let Some(path) = chosen else {
                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.set_group_image_busy(false);
                            ui.set_group_settings_status(s(""));
                        }
                    });
                    return;
                };

                let bytes = match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(e) => {
                        let msg = format!("could not read file: {e}");
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.set_group_image_busy(false);
                                ui.set_group_settings_status(msg.into());
                            }
                        });
                        return;
                    }
                };
                let content_type = mime_guess::from_path(&path)
                    .first_or_octet_stream()
                    .essence_str()
                    .to_string();

                {
                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.set_group_settings_status(s("uploading to Blossom…"));
                        }
                    });
                }

                let weak_done = weak.clone();
                let backend_cell_done = backend_cell.clone();
                let group_ids_done = group_ids.clone();
                let group_hex_done = group_hex.clone();
                let guard = backend_cell.lock().unwrap();
                let Some(backend) = guard.as_ref() else {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak_done.upgrade() {
                            ui.set_group_image_busy(false);
                            ui.set_group_settings_status(s("backend not ready"));
                        }
                    });
                    return;
                };
                backend.set_group_image_async(&group_hex, bytes, content_type, move |result| {
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = weak_done.upgrade() else {
                            return;
                        };
                        ui.set_group_image_busy(false);
                        match result {
                            Ok(_) => {
                                ui.set_group_settings_status(s("group image updated"));
                                if let Some(backend) = backend_cell_done.lock().unwrap().as_ref() {
                                    refresh_chats_async(
                                        &ui,
                                        backend,
                                        &group_ids_done,
                                        |_, _, _| {},
                                    );
                                    push_group_members_to_ui_async(&ui, backend, &group_hex_done);
                                }
                            }
                            Err(e) => {
                                eprintln!("[group-image] upload failed: {e:#}");
                                ui.set_group_settings_status(
                                    friendly_error("group image", &e).into(),
                                );
                            }
                        }
                    });
                });
            });
        }
    });
    ui.on_chat_selected({
        let weak = ui.as_weak();
        let refresh = refresh_breadcrumb.clone();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        let active_watcher = active_message_watcher.clone();
        let pending_state = pending_state.clone();
        let banner_seen = encryption_banner_seen.clone();
        let notif = notif.clone();
        let settings_cell = settings_cell.clone();
        move |idx| {
            if let Some(ui) = weak.upgrade() {
                ui.set_active_chat(idx);
                // Reply targets are per-chat; switching threads should not
                // leak a stale "Replying to …" chip across conversations.
                ui.set_reply_target_id(s(""));
                ui.set_reply_target_author(s(""));
                ui.set_reply_target_preview(s(""));
                refresh();
                let Some(backend) = backend_cell.lock().unwrap().clone() else {
                    return;
                };
                let group_hex = group_ids.lock().unwrap().get(idx as usize).cloned();
                // Reflect this chat's mute state in the header bell.
                ui.set_active_chat_muted(group_hex.as_deref().is_some_and(|g| notif.is_muted(g)));
                trigger_encryption_banner_entrance(&ui, group_hex.as_deref(), &banner_seen);
                if let Some(group_hex) = group_hex {
                    let t_switch = std::time::Instant::now();
                    // Mark the chat read: advance its read marker to now, clear
                    // its unread, persist the marker (so backlog that arrives
                    // while the app is closed surfaces as unread next launch),
                    // and clear the row's badge optimistically. Persisting on
                    // open is what makes the read state authoritative.
                    let now = now_unix_secs() as i64;
                    unread_state().set_marker(&group_hex, now);
                    unread_state().set_count(&group_hex, 0);
                    {
                        let mut st = settings_cell.borrow_mut();
                        st.last_read.insert(group_hex.clone(), now);
                        st.save();
                    }
                    clear_chat_unread_row(&ui, idx as usize);
                    refresh_unread_chrome(&ui);
                    // Re-entering a chat always starts from the default
                    // window — expanded history is per-visit.
                    msg_window_reset(&group_hex);
                    ui.set_show_chat_members(false);
                    push_group_members_to_ui_async(&ui, &backend, &group_hex);
                    // Snapshot read rides the backend runtime (sqlite can
                    // stall behind sync writes or a slow disk); rows are
                    // built back on the UI thread, merged with any pending
                    // overlay so chat switching doesn't drop pending bubbles.
                    let idx = idx as usize;
                    let my_id = backend.account().account_id_hex.clone();
                    let weak = ui.as_weak();
                    let backend_cell = backend_cell.clone();
                    let pending_state = pending_state.clone();
                    let active_watcher = active_watcher.clone();
                    let b = backend.clone();
                    backend.tokio_handle().spawn(async move {
                        let msgs = b
                            .messages(&group_hex, Some(msg_window_for(&group_hex)))
                            .unwrap_or_default();
                        let _ = slint::invoke_from_event_loop(move || {
                            let Some(ui) = weak.upgrade() else { return };
                            let chats_messages = ui.get_chats_messages();
                            {
                                let overlay = pending_state.lock().unwrap();
                                rebuild_chat_messages_from(
                                    &b,
                                    &overlay,
                                    &chats_messages,
                                    idx,
                                    &group_hex,
                                    &msgs,
                                );
                            }
                            spawn_message_avatar_fetches(&ui, &b, &msgs);
                            eprintln!(
                                "[switch-timing] chat {idx}: {} records rebuilt in {:?}",
                                msgs.len(),
                                t_switch.elapsed()
                            );
                            // Global affordances only if this chat is still
                            // the active one (rapid switches can supersede
                            // this fetch; the rows above still land in the
                            // right per-chat slot either way).
                            if ui.get_active_chat() as usize == idx {
                                ui.set_messages_has_older(msgs.len() >= MESSAGE_WINDOW);
                                // Opening a chat should land you at the most
                                // recent message, not the top of the history.
                                ui.set_messages_scroll_tick(ui.get_messages_scroll_tick() + 1);
                                // Then attach a live watcher for new arrivals
                                // (after the rebuild, so no echo lands in the
                                // gap and gets overwritten). Abort any
                                // previous one so we don't pile them up.
                                if let Some(prev) = active_watcher.lock().unwrap().take() {
                                    prev.abort();
                                }
                                let handle = install_message_watcher(
                                    &b,
                                    ui.as_weak(),
                                    backend_cell.clone(),
                                    pending_state.clone(),
                                    group_hex,
                                    idx,
                                    my_id,
                                );
                                *active_watcher.lock().unwrap() = Some(handle);
                            }
                        });
                    });
                }
            }
        }
    });
    // "Load earlier messages" at the top of the messages view: grow the
    // active chat's record window one MESSAGE_WINDOW step and rebuild. The
    // Slint side anchors the scroll so the content the user was reading
    // stays put under the newly-prepended history.
    ui.on_messages_request_older({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        let pending_state = pending_state.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let idx = ui.get_active_chat() as usize;
            let Some(group_hex) = group_ids.lock().unwrap().get(idx).cloned() else {
                return;
            };
            let Some(backend) = backend_cell.lock().unwrap().clone() else {
                return;
            };
            let new_window = msg_window_expand(&group_hex);
            // Expanded-window read on the backend runtime; rows built back on
            // the UI thread. The Slint side anchors the scroll, so the rows
            // landing a beat later keeps the content under the user.
            let weak = ui.as_weak();
            let pending_state = pending_state.clone();
            let b = backend.clone();
            backend.tokio_handle().spawn(async move {
                let msgs = b
                    .messages(&group_hex, Some(msg_window_for(&group_hex)))
                    .unwrap_or_default();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    let chats_messages = ui.get_chats_messages();
                    {
                        let overlay = pending_state.lock().unwrap();
                        rebuild_chat_messages_from(
                            &b,
                            &overlay,
                            &chats_messages,
                            idx,
                            &group_hex,
                            &msgs,
                        );
                    }
                    spawn_message_avatar_fetches(&ui, &b, &msgs);
                    if ui.get_active_chat() as usize == idx {
                        // Fewer records than asked for → the full history is
                        // loaded.
                        ui.set_messages_has_older(msgs.len() >= new_window);
                    }
                });
            });
        }
    });
    ui.on_contact_selected({
        let weak = ui.as_weak();
        let refresh = refresh_breadcrumb.clone();
        move |idx| {
            if let Some(ui) = weak.upgrade() {
                ui.set_active_contact(idx);
                refresh();
            }
        }
    });
    ui.on_archive_selected({
        let weak = ui.as_weak();
        let refresh = refresh_breadcrumb.clone();
        let backend_cell = backend_cell.clone();
        let archived_group_ids = archived_group_ids.clone();
        move |idx| {
            if let Some(ui) = weak.upgrade() {
                ui.set_active_archived(idx);
                refresh();
                let Some(backend) = backend_cell.lock().unwrap().clone() else {
                    return;
                };
                let hex = archived_group_ids
                    .lock()
                    .unwrap()
                    .get(idx as usize)
                    .cloned();
                if let Some(group_hex) = hex {
                    push_group_members_to_ui_async(&ui, &backend, &group_hex);
                }
            }
        }
    });
    ui.on_members_toggle_clicked({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_show_chat_members(!ui.get_show_chat_members());
            }
        }
    });

    // ─── Chat-request + archive actions ───────────────────────────────
    // Resolve the active chat's group hex from `group_ids` + active-chat,
    // run a backend op, then refresh both chat lists. Active-archived is
    // resolved via the archived snapshot so the index doesn't have to align
    // with `group_ids`.
    let active_chat_group_hex = {
        let weak = ui.as_weak();
        let group_ids = group_ids.clone();
        move || -> Option<String> {
            let ui = weak.upgrade()?;
            let idx = ui.get_active_chat() as usize;
            group_ids.lock().unwrap().get(idx).cloned()
        }
    };

    let refresh_all_chat_models = {
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        let archived_group_ids = archived_group_ids.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(b) = backend_cell.lock().unwrap().clone() else {
                return;
            };
            refresh_all_chat_models_async(&ui, &b, &group_ids, &archived_group_ids);
        }
    };

    ui.on_accept_chat_request({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let resolve = active_chat_group_hex.clone();
        let refresh = refresh_all_chat_models.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(group_hex) = resolve() else { return };
            let Some(b) = backend_cell.lock().unwrap().clone() else {
                ui.set_backend_error(error_copy().not_connected.into());
                return;
            };
            // Accepting publishes to relays — worker; `refresh` captures only
            // Send handles, so a clone rides into the completion.
            let weak = weak.clone();
            let refresh = refresh.clone();
            std::thread::spawn(move || {
                let result = b.accept_group_invite(&group_hex);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    if let Err(e) = result {
                        eprintln!("[accept] {e:#}");
                        ui.set_backend_error(friendly_error("accept", &e).into());
                        return;
                    }
                    refresh();
                });
            });
        }
    });

    ui.on_block_chat_request({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let resolve = active_chat_group_hex.clone();
        let refresh = refresh_all_chat_models.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(group_hex) = resolve() else { return };
            let Some(b) = backend_cell.lock().unwrap().clone() else {
                ui.set_backend_error(error_copy().not_connected.into());
                return;
            };
            let weak = weak.clone();
            let refresh = refresh.clone();
            std::thread::spawn(move || {
                let result = b.decline_group_invite(&group_hex);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    if let Err(e) = result {
                        eprintln!("[block] {e:#}");
                        ui.set_backend_error(friendly_error("block", &e).into());
                        return;
                    }
                    refresh();
                });
            });
        }
    });

    // ─── Archive / unarchive (optimistic) ──────────────────────────────
    //
    // `set_group_archived` is local-only (no relay traffic), but it still
    // sat behind a full chat-list rebuild — which scans every group and its
    // latest-message preview. On a busy account that's a perceptible hitch.
    // We do the visible work first: pull the row out of the chats model and
    // its parallel `group_ids` list, append an `ArchivedChat` entry to the
    // archived model, then let the backend catch up. On failure we put it
    // back where it was.
    ui.on_archive_chat({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let resolve = active_chat_group_hex.clone();
        let group_ids = group_ids.clone();
        let refresh = refresh_all_chat_models.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(group_hex) = resolve() else { return };

            // Locate the row in the chats model.
            let chats = ui.get_chats();
            let mut ids = group_ids.lock().unwrap();
            let Some(pos) = ids.iter().position(|g| g == &group_hex) else {
                return;
            };
            let Some(chats_vm) = chats.as_any().downcast_ref::<VecModel<ChatMeta>>() else {
                return;
            };
            let Some(removed_meta) = chats_vm.row_data(pos) else {
                return;
            };

            // 1. Optimistic UI mutation. Drop the chat row + its parallel
            //    messages model + its id. Append an `ArchivedChat` shaped
            //    from the existing ChatMeta so the archive page reflects it
            //    without waiting on a backend snapshot.
            chats_vm.remove(pos);
            let chats_messages = ui.get_chats_messages();
            if let Some(outer_vm) = chats_messages
                .as_any()
                .downcast_ref::<VecModel<ModelRc<ChatMessage>>>()
                && pos < outer_vm.row_count()
            {
                outer_vm.remove(pos);
            }
            ids.remove(pos);
            let archived_row = ArchivedChat {
                name: removed_meta.name.clone(),
                last_msg: removed_meta.preview.clone(),
                last_date: removed_meta.stamp.clone(),
                av_a: removed_meta.av_a,
                av_b: removed_meta.av_b,
                av_initials: removed_meta.av_initials.clone(),
                members: 0,
                group_id: removed_meta.npub.clone(),
                picture: removed_meta.picture.clone(),
                has_picture: removed_meta.has_picture,
            };
            if let Some(archived_vm) = ui
                .get_archived_chats()
                .as_any()
                .downcast_ref::<VecModel<ArchivedChat>>()
            {
                archived_vm.push(archived_row);
            }
            let new_len = chats_vm.row_count() as i32;
            if ui.get_active_chat() >= new_len {
                ui.set_active_chat((new_len - 1).max(0));
            }
            drop(ids);

            // 2. Commit on a worker thread — `set_group_archived` is a
            //    synchronous disk write. Posting it off the UI thread is
            //    the difference between "instant" and the perceptible hitch
            //    Danny saw. On failure we hop back, surface the error, and
            //    fall back to a full refresh to reconcile.
            let weak_cb = weak.clone();
            let backend_cell = backend_cell.clone();
            let group_hex_cb = group_hex.clone();
            let refresh_cb = refresh.clone();
            std::thread::spawn(move || {
                let res = {
                    let guard = backend_cell.lock().unwrap();
                    guard
                        .as_ref()
                        .map(|b| b.set_group_archived(&group_hex_cb, true))
                };
                if let Some(Err(e)) = res {
                    eprintln!("[archive] {e:#}");
                    let refresh_cb = refresh_cb.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = weak_cb.upgrade() else { return };
                        ui.set_backend_error(friendly_error("archive", &e).into());
                        refresh_cb();
                    });
                }
            });
        }
    });

    ui.on_unarchive_chat({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let refresh = refresh_all_chat_models.clone();
        let group_ids = group_ids.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let idx = ui.get_active_archived() as usize;
            let Some(b) = backend_cell.lock().unwrap().clone() else {
                ui.set_backend_error(error_copy().not_connected.into());
                return;
            };

            // Resolve the real group id via the backend's archived snapshot
            // (a sqlite read — runtime, not UI thread). ArchivedChat.group_id
            // is rendered as "mls:0x<short>", hence the round-trip.
            let weak = weak.clone();
            let group_ids = group_ids.clone();
            let refresh = refresh.clone();
            let backend_cell = backend_cell.clone();
            let b2 = b.clone();
            b.tokio_handle().spawn(async move {
                let Ok(records) = b2.archived_chats() else {
                    return;
                };
                let Some(record) = records.get(idx).cloned() else {
                    return;
                };
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    let my_id = b2.account().account_id_hex.clone();
                    // Unread starts at 0 on the optimistic unarchive row (no
                    // UI-thread message scan); the next chat-list snapshot
                    // recomputes it from the read marker.
                    let meta_from_record = chat_meta_from(&record, None, &my_id, &b2, 0);
                    let group_hex = record.group_id_hex.clone();

                    // 1. Optimistic: pop the archived row, push the chat back
                    //    into the chats model.
                    let archived_model = ui.get_archived_chats();
                    if let Some(vm) = archived_model
                        .as_any()
                        .downcast_ref::<VecModel<ArchivedChat>>()
                        && idx < vm.row_count()
                    {
                        vm.remove(idx);
                    }
                    if let Some(chats_vm) =
                        ui.get_chats().as_any().downcast_ref::<VecModel<ChatMeta>>()
                    {
                        chats_vm.push(meta_from_record);
                    }
                    if let Some(outer_vm) = ui
                        .get_chats_messages()
                        .as_any()
                        .downcast_ref::<VecModel<ModelRc<ChatMessage>>>()
                    {
                        outer_vm.push(ModelRc::new(VecModel::from(Vec::<ChatMessage>::new())));
                    }
                    {
                        let mut ids = group_ids.lock().unwrap();
                        ids.push(group_hex.clone());
                    }
                    let alen = archived_model.row_count() as i32;
                    if ui.get_active_archived() >= alen {
                        ui.set_active_archived((alen - 1).max(0));
                    }

                    // 2. Commit on a worker thread; reconcile with a full
                    //    refresh on failure.
                    let weak_cb = weak.clone();
                    let backend_cell = backend_cell.clone();
                    let group_hex_cb = group_hex.clone();
                    let refresh_cb = refresh.clone();
                    std::thread::spawn(move || {
                        let res = {
                            let guard = backend_cell.lock().unwrap();
                            guard
                                .as_ref()
                                .map(|b| b.set_group_archived(&group_hex_cb, false))
                        };
                        if let Some(Err(e)) = res {
                            eprintln!("[unarchive] {e:#}");
                            let refresh_cb = refresh_cb.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                let Some(ui) = weak_cb.upgrade() else { return };
                                ui.set_backend_error(friendly_error("unarchive", &e).into());
                                refresh_cb();
                            });
                        }
                    });
                });
            });
        }
    });

    // ─── Command palette wiring ────────────────────────────────────────
    let palette_master = all_palette_actions();

    // Ctrl+K: populate actions for the empty query and open the palette.
    ui.on_palette_requested({
        let weak = ui.as_weak();
        let master = palette_master.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_palette_query(s(""));
            ui.set_palette_actions(model(filter_palette(&master, "")));
            ui.set_show_palette(true);
        }
    });

    ui.on_palette_dismissed({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_show_palette(false);
            }
        }
    });

    ui.on_palette_query_changed({
        let weak = ui.as_weak();
        let master = palette_master.clone();
        move |q| {
            if let Some(ui) = weak.upgrade() {
                ui.set_palette_actions(model(filter_palette(&master, q.as_str())));
            }
        }
    });

    ui.on_palette_execute({
        let weak = ui.as_weak();
        let go = go_to_page.clone();
        let settings_cell = settings_cell.clone();
        move |id| {
            let Some(ui) = weak.upgrade() else { return };
            match id.as_str() {
                "nav.chats" => go(Page::Chats),
                "nav.contacts" => go(Page::Contacts),
                "nav.archived" => go(Page::Archived),
                "nav.keys" => go(Page::Keys),
                "nav.settings" => go(Page::Settings),
                "nav.profile" => go(Page::Profile),
                "act.new-chat" => ui.set_show_new_chat(true),
                "act.copy-npub" => {
                    let npub = ui.get_my_npub();
                    copy_to_clipboard_async(npub.to_string(), |result| {
                        if let Err(e) = result {
                            eprintln!("[clipboard] copy npub failed: {e}");
                        }
                    });
                }
                "act.toggle-retro" => {
                    let mode = if ui.get_retro_mode() { "dark" } else { "retro" };
                    {
                        let mut s = settings_cell.borrow_mut();
                        s.theme = mode.into();
                        s.save();
                    }
                    apply_theme_mode(&ui, mode);
                }
                _ => {}
            }
        }
    });

    // ─── Send message (optimistic) ─────────────────────────────────────
    //
    // Flow:
    //   1. Insert pending bubble + clear draft instantly.
    //   2. Spawn the real send on tokio (non-blocking).
    //   3. On ack from the runtime, hop back to the Slint event loop, drop
    //      the pending entry, and rebuild from the backend snapshot — which
    //      now contains the real record.
    //   4. On failure, mark the pending entry failed and rebuild (the row
    //      stays put but flips to the red "tap to retry" state).
    //
    // The UI never blocks on the network. The pending bubble dims + shows
    // a single check; once confirmed it flips to the regular double-check.
    // Signature: (group_hex, text, temp_id, Option<parent_id_hex>). When the
    // parent id is `Some`, the dispatch routes through `reply_text_async` so
    // the wire event carries `e`+`q` tags; otherwise it's a vanilla send.
    let dispatch_send: Rc<dyn Fn(String, String, String, Option<String>)> = {
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        let pending_state = pending_state.clone();
        Rc::new(
            move |group_hex: String, text: String, temp_id: String, parent_id: Option<String>| {
                let guard = backend_cell.lock().unwrap();
                let Some(backend) = guard.as_ref() else {
                    return;
                };
                // Mark in flight so the reconnect flush won't dispatch it again
                // concurrently. Cleared when this send resolves (ack or error).
                offline_inflight_insert(&temp_id);
                let weak_cb = weak.clone();
                let group_ids_cb = group_ids.clone();
                let pending_state_cb = pending_state.clone();
                let backend_cell_cb = backend_cell.clone();
                let group_hex_cb = group_hex.clone();
                let temp_id_cb = temp_id.clone();
                let on_done = move |result: anyhow::Result<marmot_app::SendSummary>| {
                    // Tokio worker. `ModelRc` is `!Send` — look it up off the UI
                    // handle inside the invoke closure. The window snapshot is
                    // read HERE so the invoke closure never touches sqlite.
                    let weak = weak_cb.clone();
                    let group_ids = group_ids_cb.clone();
                    let pending_state = pending_state_cb.clone();
                    let backend_cell = backend_cell_cb.clone();
                    let group_hex = group_hex_cb.clone();
                    let temp_id = temp_id_cb.clone();
                    // This send has resolved — drop the in-flight guard.
                    offline_inflight_remove(&temp_id);
                    // On a failure, decide here (on the worker thread, where a
                    // blocking `relay_health` poll is fine) whether we're offline.
                    // An offline failure keeps the bubble *pending* and the durable
                    // entry queued for the reconnect flush; an online failure is a
                    // real error and flips the bubble red.
                    let (all, online): (Vec<AppMessageRecord>, bool) = if result.is_ok() {
                        let all = backend_cell
                            .lock()
                            .unwrap()
                            .as_ref()
                            .map(|b| {
                                b.messages(&group_hex, Some(msg_window_for(&group_hex)))
                                    .unwrap_or_default()
                            })
                            .unwrap_or_default();
                        (all, true)
                    } else {
                        let online = backend_cell
                            .lock()
                            .unwrap()
                            .as_ref()
                            .map(|b| b.relay_health().0 > 0)
                            .unwrap_or(false);
                        (Vec::new(), online)
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = weak.upgrade() else { return };
                        let ids = group_ids.lock().unwrap();
                        let Some(idx) = ids.iter().position(|g| g == &group_hex) else {
                            return;
                        };
                        let chats_messages = ui.get_chats_messages();

                        match result {
                            Ok(summary) => {
                                // Surgical reconciliation: find the pending row,
                                // build the confirmed row from the backend record,
                                // and swap that single row. Siblings don't remount.
                                let real_id = summary.message_ids.first().cloned();
                                pending_state
                                    .lock()
                                    .unwrap()
                                    .drop_send(&group_hex, &temp_id);
                                // Confirmed — drop the durable queue entry.
                                offline_queue::remove(&temp_id);

                                let guard = backend_cell.lock().unwrap();
                                let Some(backend) = guard.as_ref() else {
                                    return;
                                };
                                let overlay = pending_state.lock().unwrap();
                                let my_id = backend.account().account_id_hex.clone();
                                let my_label = my_avatar_label(backend, &my_id);

                                let confirmed_row: Option<ChatMessage> =
                                    real_id.as_deref().and_then(|id| {
                                        let rec =
                                            all.iter().find(|m| m.message_id_hex == id).cloned()?;
                                        Some(build_one_message_row(
                                            &rec, &all, &my_id, &my_label, &group_hex, &overlay,
                                            backend,
                                        ))
                                    });

                                let swapped = with_inner_messages(&chats_messages, idx, |vm| {
                                    let Some(pos) = find_message_row(vm, &temp_id) else {
                                        return false;
                                    };
                                    if let Some(mut row) = confirmed_row {
                                        // Keep the grouping the pending row had so a
                                        // confirmed send doesn't pop its avatar back.
                                        preserve_grouping_flags(vm, pos, &mut row);
                                        vm.set_row_data(pos, row);
                                    } else {
                                        // No real id came back — just remove the
                                        // pending placeholder; the watcher will
                                        // append the real row when it echoes.
                                        vm.remove(pos);
                                    }
                                    true
                                });

                                // Fallback: if the model wasn't shaped how we
                                // expected, do a full rebuild rather than silently
                                // lose the pending row.
                                if swapped != Some(true) {
                                    rebuild_chat_messages_from(
                                        backend,
                                        &overlay,
                                        &chats_messages,
                                        idx,
                                        &group_hex,
                                        &all,
                                    );
                                }
                            }
                            Err(e) => {
                                eprintln!("[send] {e:#}");
                                if !online {
                                    // Offline: leave the bubble pending ("sending…")
                                    // and the durable entry queued. The reconnect
                                    // flush re-dispatches it automatically.
                                    eprintln!("[send] offline — left queued for flush");
                                    return;
                                }
                                ui.set_backend_error(friendly_error("send", &e).into());
                                // Online failure: a real error. Mark failed in place
                                // — the bubble flips to red without disturbing its
                                // neighbours.
                                let mut overlay = pending_state.lock().unwrap();
                                overlay.mark_send_failed(&group_hex, &temp_id);
                                let failed_send = overlay.find_send(&group_hex, &temp_id);
                                drop(overlay);
                                if let Some(failed) = failed_send {
                                    let guard = backend_cell.lock().unwrap();
                                    let Some(backend) = guard.as_ref() else {
                                        return;
                                    };
                                    let my_id = backend.account().account_id_hex.clone();
                                    let my_label = my_avatar_label(backend, &my_id);
                                    let _ = with_inner_messages(&chats_messages, idx, |vm| {
                                        if let Some(pos) = find_message_row(vm, &temp_id) {
                                            let mut row =
                                                pending_chat_message(&failed, &my_id, &my_label);
                                            preserve_grouping_flags(vm, pos, &mut row);
                                            vm.set_row_data(pos, row);
                                        }
                                    });
                                }
                            }
                        }
                    });
                };
                match parent_id {
                    Some(parent) => {
                        backend.reply_text_async(&group_hex, &parent, &text, on_done);
                    }
                    None => {
                        backend.send_text_async(&group_hex, &text, on_done);
                    }
                }
            },
        )
    };

    // ─── Edit dispatch (optimistic, surgical) ─────────────────────────
    //
    // Same shape as `react_op`: stamp the overlay, rewrite ONLY the target
    // bubble's text locally, publish the kind-1009 in the background, then on
    // ack drop the overlay and refresh ONLY that row from the snapshot (which
    // now carries the confirmed edit). On failure the overlay is dropped too,
    // so the row reverts to its last confirmed text.
    let edit_op: Rc<dyn Fn(String, String)> = {
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        let pending_state = pending_state.clone();
        Rc::new(move |target: String, text: String| {
            let Some(ui) = weak.upgrade() else { return };
            let idx = ui.get_active_chat() as usize;
            let Some(group_hex) = group_ids.lock().unwrap().get(idx).cloned() else {
                return;
            };
            let chats_messages = ui.get_chats_messages();

            // 1. Optimistic overlay + model-only row rewrite.
            {
                let mut overlay = pending_state.lock().unwrap();
                overlay
                    .edits
                    .insert((group_hex.clone(), target.clone()), text.clone());
            }
            apply_edit_to_model_row(&chats_messages, idx, &target, &text);

            // 2. Dispatch + reconcile (also surgical).
            let guard = backend_cell.lock().unwrap();
            let Some(backend) = guard.as_ref() else {
                return;
            };
            let weak_cb = weak.clone();
            let group_ids_cb = group_ids.clone();
            let pending_state_cb = pending_state.clone();
            let backend_cell_cb = backend_cell.clone();
            let group_hex_cb = group_hex.clone();
            let target_cb = target.clone();
            let on_done = move |result: anyhow::Result<marmot_app::SendSummary>| {
                let weak = weak_cb.clone();
                let group_ids = group_ids_cb.clone();
                let pending_state = pending_state_cb.clone();
                let backend_cell = backend_cell_cb.clone();
                let group_hex = group_hex_cb.clone();
                let target = target_cb.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    {
                        let mut overlay = pending_state.lock().unwrap();
                        if let Err(e) = &result {
                            eprintln!("[edit] {e:#}");
                            ui.set_backend_error(friendly_error("edit", e).into());
                        }
                        overlay.edits.remove(&(group_hex.clone(), target.clone()));
                    }
                    let Some(backend) = backend_cell.lock().unwrap().clone() else {
                        return;
                    };
                    // Snapshot read + row rebuild ride the backend runtime —
                    // no sqlite on the UI thread.
                    refresh_one_message_row_async(
                        &backend,
                        ui.as_weak(),
                        pending_state.clone(),
                        group_ids.clone(),
                        group_hex,
                        target,
                    );
                });
            };
            backend.edit_message_async(&group_hex, &target, &text, on_done);
        })
    };

    ui.on_send_message({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        let chats_messages = chats_messages.clone();
        let pending_state = pending_state.clone();
        let staged_files = staged_files.clone();
        let dispatch_send = dispatch_send.clone();
        let edit_op = edit_op.clone();
        let vault_cell = vault_cell.clone();
        move |text| {
            let Some(ui) = weak.upgrade() else { return };
            // A send closes the mention picker; the draft is about to clear.
            ui.set_mention_active(false);
            let text = text.trim().to_string();
            // Edit mode: when an edit target is set, this "send" rewrites that
            // message via a kind-1009 instead of posting a new one. Clear the
            // edit state + composer first so the banner drops immediately.
            // (Staged attachments stay queued — an edit never sends them.)
            let editing_id = ui.get_editing_message_id().to_string();
            if !editing_id.is_empty() {
                if text.is_empty() {
                    return;
                }
                ui.set_editing_message_id(s(""));
                ui.set_composer_draft(s(""));
                edit_op(editing_id, text);
                return;
            }
            if text.is_empty() && staged_files.lock().unwrap().is_empty() {
                return;
            }
            let idx = ui.get_active_chat() as usize;
            let Some(group_hex) = group_ids.lock().unwrap().get(idx).cloned() else {
                return;
            };
            let guard = backend_cell.lock().unwrap();
            let Some(backend) = guard.as_ref() else {
                ui.set_backend_error(error_copy().not_connected.into());
                return;
            };

            if !text.is_empty() {
                // Armed message effect (Telegram-style). Read + disarm it now so
                // it rides this one send; the marker travels in the wire body so
                // the recipient replays the same burst.
                let effect_id = ui.global::<EffectCatalog>().get_selected();
                ui.global::<EffectCatalog>().set_selected(0);
                // Snapshot + clear the reply target (if any) so this send goes
                // out as a reply once and only once. The chip disappears as soon
                // as the user presses send — matches Telegram / Slack feel.
                let reply_target_id = ui.get_reply_target_id().to_string();
                let reply_to = if reply_target_id.is_empty() {
                    None
                } else {
                    Some((
                        reply_target_id.clone(),
                        ui.get_reply_target_author().to_string(),
                        ui.get_reply_target_preview().to_string(),
                    ))
                };
                if reply_to.is_some() {
                    ui.set_reply_target_id(s(""));
                    ui.set_reply_target_author(s(""));
                    ui.set_reply_target_preview(s(""));
                }

                // 1. Insert pending bubble + clear the composer. Surgical push —
                //    no full rebuild, no neighbour remount.
                let temp_id = next_temp_id();
                let send = PendingSend {
                    temp_id: temp_id.clone(),
                    text: text.clone(),
                    failed: false,
                    reply_to: reply_to.clone(),
                    media: Vec::new(),
                    effect: effect_id,
                };
                {
                    let mut overlay = pending_state.lock().unwrap();
                    overlay.add_send(&group_hex, send.clone());
                }
                let my_id = backend.account().account_id_hex.clone();
                let my_label = my_avatar_label(backend, &my_id);
                // Durably queue this send so it survives a restart and auto-flushes
                // on reconnect. The disk entry carries the *clean* text +
                // effect id; the wire body's effect marker is reconstructed at
                // (re)dispatch time.
                offline_persist(
                    &vault_cell,
                    &offline_queue::QueuedSend {
                        temp_id: temp_id.clone(),
                        account_id_hex: my_id.clone(),
                        group_hex: group_hex.clone(),
                        kind: offline_queue::QueuedKind::Text {
                            text: text.clone(),
                            reply_to: reply_to.clone(),
                            effect: effect_id,
                        },
                        enqueued_at: offline_queue::now_secs(),
                    },
                );
                let pending_row = pending_chat_message(&send, &my_id, &my_label);
                with_inner_messages(&chats_messages, idx, |vm| {
                    push_message_grouped(vm, pending_row)
                });
                ui.set_composer_draft(s(""));
                // Force-scroll to the new bubble. The MessagesArea watches this
                // tick and animates viewport-y to the bottom — so the user sees
                // their message even if they were paged up reading history.
                ui.set_messages_scroll_tick(ui.get_messages_scroll_tick() + 1);
                drop(guard);

                // 2. Dispatch the real send in the background. The wire body
                //    carries the effect marker (if any); the pending row kept the
                //    clean text.
                let parent_id = reply_to.as_ref().map(|(id, _, _)| id.clone());
                dispatch_send(
                    group_hex.clone(),
                    append_effect_marker(&text, effect_id),
                    temp_id,
                    parent_id,
                );
            } else {
                drop(guard);
            }

            // 3. Flush the staged attachments. Multiple images go out as one
            //    kind-9 album (one bubble, rendered as a grid); a lone image or
            //    any non-image file goes out as its own message. Chips clear
            //    immediately; a failed upload surfaces on its bubble (red, tap
            //    to retry) like any other send. Telegram caps an album at 10.
            let staged_now: Vec<StagedFile> = std::mem::take(&mut *staged_files.lock().unwrap());
            if !staged_now.is_empty() {
                refresh_staged_ui(&ui, &[]);
                let (images, others): (Vec<StagedFile>, Vec<StagedFile>) =
                    staged_now.into_iter().partition(|f| f.is_image);
                // Images: one album per chunk of 10; a single leftover image
                // falls through to the single-attachment path.
                for chunk in images.chunks(10) {
                    if chunk.len() == 1 {
                        let f = chunk[0].clone();
                        spawn_attachment_send(
                            weak.clone(),
                            backend_cell.clone(),
                            group_ids.clone(),
                            pending_state.clone(),
                            vault_cell.clone(),
                            group_hex.clone(),
                            f.file_name,
                            f.media_type,
                            f.bytes,
                            f.is_image,
                            f.preview,
                            None,
                        );
                    } else {
                        spawn_album_send(
                            weak.clone(),
                            backend_cell.clone(),
                            group_ids.clone(),
                            pending_state.clone(),
                            vault_cell.clone(),
                            group_hex.clone(),
                            chunk.to_vec(),
                            None,
                        );
                    }
                }
                for f in others {
                    spawn_attachment_send(
                        weak.clone(),
                        backend_cell.clone(),
                        group_ids.clone(),
                        pending_state.clone(),
                        vault_cell.clone(),
                        group_hex.clone(),
                        f.file_name,
                        f.media_type,
                        f.bytes,
                        f.is_image,
                        f.preview,
                        None,
                    );
                }
            }
        }
    });

    // ─── Retry a failed send ───────────────────────────────────────────
    //
    // The bubble owns its retry click. We look up the pending entry by its
    // temp id (carried in `message_id`), flip it back to non-failed, and
    // re-dispatch.
    ui.on_retry_message({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        let chats_messages = chats_messages.clone();
        let pending_state = pending_state.clone();
        let dispatch_send = dispatch_send.clone();
        let vault_cell = vault_cell.clone();
        move |message_id| {
            let Some(ui) = weak.upgrade() else { return };
            let temp_id = message_id.to_string();
            let idx = ui.get_active_chat() as usize;
            let Some(group_hex) = group_ids.lock().unwrap().get(idx).cloned() else {
                return;
            };
            let send = {
                let overlay = pending_state.lock().unwrap();
                overlay.find_send(&group_hex, &temp_id)
            };
            let Some(mut send) = send else { return };
            send.failed = false;
            {
                let mut overlay = pending_state.lock().unwrap();
                if let Some(v) = overlay.sends.get_mut(&group_hex) {
                    for p in v.iter_mut() {
                        if p.temp_id == temp_id {
                            p.failed = false;
                        }
                    }
                }
            }
            // Surgical flip: just rewrite the failed row back to pending.
            let guard = backend_cell.lock().unwrap();
            if let Some(backend) = guard.as_ref() {
                let my_id = backend.account().account_id_hex.clone();
                let my_label = my_avatar_label(backend, &my_id);
                let mut row = pending_chat_message(&send, &my_id, &my_label);
                with_inner_messages(&chats_messages, idx, |vm| {
                    if let Some(pos) = find_message_row(vm, &temp_id) {
                        preserve_grouping_flags(vm, pos, &mut row);
                        vm.set_row_data(pos, row);
                    }
                });
            }
            drop(guard);
            // Re-dispatch. Plain text/reply goes through the normal send path; a
            // media send can't (its bytes aren't in the overlay), so recover them
            // from the durable queue and replay the upload under the same temp id.
            if send.media.is_empty() {
                offline_inflight_insert(&temp_id);
                let parent_id = send.reply_to.as_ref().map(|(id, _, _)| id.clone());
                dispatch_send(group_hex, send.text, temp_id, parent_id);
            } else {
                let entry = vault_cell
                    .lock()
                    .ok()
                    .and_then(|g| g.clone())
                    .and_then(|v| offline_queue::load_one(&v, &temp_id));
                match entry.map(|e| e.kind) {
                    Some(offline_queue::QueuedKind::Attachment(m)) => {
                        offline_inflight_insert(&temp_id);
                        spawn_attachment_send(
                            weak.clone(),
                            backend_cell.clone(),
                            group_ids.clone(),
                            pending_state.clone(),
                            vault_cell.clone(),
                            group_hex,
                            m.file_name,
                            m.media_type,
                            m.bytes,
                            m.is_image,
                            None,
                            Some(temp_id),
                        );
                    }
                    Some(offline_queue::QueuedKind::Album(ms)) => {
                        offline_inflight_insert(&temp_id);
                        let files: Vec<StagedFile> = ms
                            .into_iter()
                            .map(|m| StagedFile {
                                file_name: m.file_name,
                                media_type: m.media_type,
                                bytes: m.bytes,
                                is_image: m.is_image,
                                preview: None,
                                thumb: None,
                            })
                            .collect();
                        spawn_album_send(
                            weak.clone(),
                            backend_cell.clone(),
                            group_ids.clone(),
                            pending_state.clone(),
                            vault_cell.clone(),
                            group_hex,
                            files,
                            Some(temp_id),
                        );
                    }
                    _ => {
                        // No durable bytes to retry with (e.g. an entry from before
                        // this feature). Leave the bubble as-is.
                        eprintln!("[retry] no durable media for {temp_id}");
                    }
                }
            }
        }
    });

    // ─── Attach file ───────────────────────────────────────────────────
    //
    // Composer paperclip → portal file picker → *staged* attachment chips.
    // Nothing uploads here: picked files (multi-select) are read + decoded
    // off-UI and appended to `staged_files`; the chip row above the input
    // is the user's confirmation, and `on_send_message` flushes the queue
    // through `spawn_attachment_send` when Send is pressed.
    ui.on_attach_file({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let staged_files = staged_files.clone();
        move || {
            let guard = backend_cell.lock().unwrap();
            let Some(backend) = guard.as_ref() else {
                return;
            };
            let tokio_handle = backend.tokio_handle();
            drop(guard);

            let weak_t = weak.clone();
            let staged_t = staged_files.clone();

            // rfd's xdg-portal backend drives ashpd/zbus. We use the
            // async-std executor flavor of zbus (not tokio) so zbus's own
            // internal connection thread runs its own reactor — no tokio
            // context juggling required. The sync rfd call still goes on a
            // blocking thread so we don't stall a tokio worker.
            tokio_handle.spawn(async move {
                let picked = match tokio::task::spawn_blocking(move || {
                    rfd::FileDialog::new()
                        .set_title("Attach files")
                        .pick_files()
                })
                .await
                {
                    Ok(Some(p)) => p,
                    Ok(None) => return,
                    Err(e) => {
                        eprintln!("[attach] picker join: {e:#}");
                        return;
                    }
                };
                let mut new_files: Vec<StagedFile> = Vec::new();
                for path in picked {
                    let file_name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "attachment".to_string());
                    let media_type = mime_guess::from_path(&path)
                        .first_or_octet_stream()
                        .essence_str()
                        .to_string();
                    let path_for_read = path.clone();
                    // Read + image decode on a blocking thread; a file that
                    // fails to read is skipped, not fatal to the batch.
                    match tokio::task::spawn_blocking(move || {
                        std::fs::read(&path_for_read)
                            .map(|bytes| staged_file_from_bytes(file_name, media_type, bytes))
                    })
                    .await
                    {
                        Ok(Ok(f)) => new_files.push(f),
                        Ok(Err(e)) => eprintln!("[attach] read {}: {e:#}", path.display()),
                        Err(e) => eprintln!("[attach] read join: {e:#}"),
                    }
                }
                if new_files.is_empty() {
                    return;
                }
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak_t.upgrade() else { return };
                    let mut staged = staged_t.lock().unwrap();
                    staged.extend(new_files);
                    refresh_staged_ui(&ui, &staged);
                });
            });
        }
    });

    // ─── Attachment send (shared tail) ─────────────────────────────────
    //
    // Called by `on_send_message` for each staged attachment: insert the
    // optimistic pending bubble, run the encrypted Blossom upload + kind-9
    // publish, reconcile the bubble when the round-trip resolves. A nested
    // item (captures nothing) so it lives next to the flows that feed it.
    //
    // Thread-safety: `ModelRc` is `!Send`, so we never carry it across the
    // tokio boundary — every closure that hops back to the UI re-fetches
    // the model via `ui.get_chats_messages()`.
    // `replay_temp_id` is `None` for a fresh send (allocate an id, render the
    // pending bubble, persist a durable queue entry) and `Some(id)` when the
    // reconnect flush re-dispatches an already-queued attachment using bytes read
    // back from disk — in which case the overlay entry/bubble may already
    // exist and the durable entry is already on disk.
    #[allow(clippy::too_many_arguments)]
    fn spawn_attachment_send(
        weak: slint::Weak<DarkMatterLinux>,
        backend_cell: Arc<Mutex<Option<Arc<Backend>>>>,
        group_ids: Arc<Mutex<Vec<String>>>,
        pending_state: Arc<Mutex<PendingState>>,
        vault_cell: Arc<Mutex<Option<Arc<Mutex<Vault>>>>>,
        group_hex: String,
        file_name: String,
        media_type: String,
        bytes: Vec<u8>,
        is_image: bool,
        local_preview: Option<PicturePixels>,
        replay_temp_id: Option<String>,
    ) {
        let size_bytes = bytes.len() as u64;
        let weak2 = weak;
        let backend_cell2 = backend_cell;
        let group_ids2 = group_ids;
        let pending_state2 = pending_state;
        let group_hex2 = group_hex;
        let file_name_u = file_name.clone();
        let media_type_u = media_type.clone();
        let bytes_for_queue = bytes.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak2.upgrade() else { return };
            let chats_messages = ui.get_chats_messages();
            let ids = group_ids2.lock().unwrap();
            let Some(idx) = ids.iter().position(|g| g == &group_hex2) else {
                return;
            };
            drop(ids);
            let guard = backend_cell2.lock().unwrap();
            let Some(backend) = guard.as_ref() else {
                return;
            };

            let is_replay = replay_temp_id.is_some();
            let temp_id = replay_temp_id.unwrap_or_else(next_temp_id);
            let send = PendingSend {
                temp_id: temp_id.clone(),
                text: String::new(),
                failed: false,
                reply_to: None,
                effect: 0,
                media: vec![PendingMedia {
                    file_name: file_name_u.clone(),
                    media_type: media_type_u.clone(),
                    size_bytes,
                    is_image,
                    is_video: mime_is_video(&media_type_u),
                    is_audio: mime_is_audio(&media_type_u),
                    local_preview: local_preview.clone(),
                }],
            };
            // Render the pending bubble + insert the overlay entry only if it isn't
            // already present (a replay of an in-session offline failure keeps its
            // existing bubble; a boot replay has none yet).
            let already_present = pending_state2
                .lock()
                .unwrap()
                .find_send(&group_hex2, &temp_id)
                .is_some();
            if !already_present {
                {
                    let mut overlay = pending_state2.lock().unwrap();
                    overlay.add_send(&group_hex2, send.clone());
                }
                let my_id = backend.account().account_id_hex.clone();
                let my_label = my_avatar_label(backend, &my_id);
                let pending_row = pending_chat_message(&send, &my_id, &my_label);
                with_inner_messages(&chats_messages, idx, |vm| {
                    push_message_grouped(vm, pending_row)
                });
                ui.set_messages_scroll_tick(ui.get_messages_scroll_tick() + 1);
            }
            // Durably queue this attachment on first send so it survives a restart.
            if !is_replay {
                let my_id = backend.account().account_id_hex.clone();
                offline_persist(
                    &vault_cell,
                    &offline_queue::QueuedSend {
                        temp_id: temp_id.clone(),
                        account_id_hex: my_id,
                        group_hex: group_hex2.clone(),
                        kind: offline_queue::QueuedKind::Attachment(offline_queue::QueuedMedia {
                            file_name: file_name_u.clone(),
                            media_type: media_type_u.clone(),
                            bytes: bytes_for_queue,
                            is_image,
                        }),
                        enqueued_at: offline_queue::now_secs(),
                    },
                );
            }
            offline_inflight_insert(&temp_id);

            let weak3 = weak2.clone();
            let backend_cell3 = backend_cell2.clone();
            let group_ids3 = group_ids2.clone();
            let pending_state3 = pending_state2.clone();
            let group_hex3 = group_hex2.clone();
            let temp_id3 = temp_id.clone();
            let local_preview_done = local_preview.clone();
            backend.upload_media_async(
                &group_hex2,
                file_name,
                media_type,
                bytes,
                None,
                move |result| {
                    let weak = weak3.clone();
                    let backend_cell = backend_cell3.clone();
                    let group_ids = group_ids3.clone();
                    let pending_state = pending_state3.clone();
                    let group_hex = group_hex3.clone();
                    let temp_id = temp_id3.clone();
                    let local_preview = local_preview_done.clone();
                    // This upload has resolved — drop the in-flight guard.
                    offline_inflight_remove(&temp_id);
                    // Tokio worker — read the refreshed window HERE
                    // so the invoke closure never touches sqlite. On failure also
                    // poll connectivity so an offline failure stays queued + pending
                    // rather than flipping the bubble red.
                    let (all, online): (Vec<AppMessageRecord>, bool) = if result.is_ok() {
                        let all = backend_cell
                            .lock()
                            .unwrap()
                            .as_ref()
                            .map(|b| {
                                b.messages(&group_hex, Some(msg_window_for(&group_hex)))
                                    .unwrap_or_default()
                            })
                            .unwrap_or_default();
                        (all, true)
                    } else {
                        let online = backend_cell
                            .lock()
                            .unwrap()
                            .as_ref()
                            .map(|b| b.relay_health().0 > 0)
                            .unwrap_or(false);
                        (Vec::new(), online)
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = weak.upgrade() else { return };
                        let chats_messages = ui.get_chats_messages();
                        let ids = group_ids.lock().unwrap();
                        let Some(idx) = ids.iter().position(|g| g == &group_hex) else {
                            return;
                        };
                        drop(ids);

                        match result {
                            Ok(upload) => {
                                pending_state
                                    .lock()
                                    .unwrap()
                                    .drop_send(&group_hex, &temp_id);
                                offline_queue::remove(&temp_id);
                                let guard = backend_cell.lock().unwrap();
                                let Some(backend) = guard.as_ref() else {
                                    return;
                                };
                                let real_id = upload
                                    .sent
                                    .as_ref()
                                    .and_then(|s| s.message_ids.first().cloned());
                                if let (Some(id), Some(p)) =
                                    (real_id.as_ref(), local_preview.as_ref())
                                    && is_image
                                {
                                    attachment_image_cache_put(id.clone(), p.clone());
                                }
                                let confirmed_row: Option<ChatMessage> =
                                    real_id.as_deref().and_then(|id| {
                                        let rec =
                                            all.iter().find(|m| m.message_id_hex == id).cloned()?;
                                        let overlay = pending_state.lock().unwrap();
                                        let my_id = backend.account().account_id_hex.clone();
                                        let my_label = my_avatar_label(backend, &my_id);
                                        Some(build_one_message_row(
                                            &rec, &all, &my_id, &my_label, &group_hex, &overlay,
                                            backend,
                                        ))
                                    });
                                let swapped = with_inner_messages(&chats_messages, idx, |vm| {
                                    let Some(pos) = find_message_row(vm, &temp_id) else {
                                        return false;
                                    };
                                    if let Some(mut row) = confirmed_row {
                                        preserve_grouping_flags(vm, pos, &mut row);
                                        vm.set_row_data(pos, row);
                                    } else {
                                        vm.remove(pos);
                                    }
                                    true
                                });
                                if swapped != Some(true) {
                                    let overlay = pending_state.lock().unwrap();
                                    rebuild_chat_messages_from(
                                        backend,
                                        &overlay,
                                        &chats_messages,
                                        idx,
                                        &group_hex,
                                        &all,
                                    );
                                }
                            }
                            Err(e) => {
                                eprintln!("[attach] upload: {e:#}");
                                if !online {
                                    // Offline: keep the bubble pending + the entry
                                    // queued for the reconnect flush.
                                    eprintln!("[attach] offline — left queued for flush");
                                    return;
                                }
                                let mut overlay = pending_state.lock().unwrap();
                                overlay.mark_send_failed(&group_hex, &temp_id);
                                let failed = overlay.find_send(&group_hex, &temp_id);
                                drop(overlay);
                                if let Some(failed) = failed {
                                    let guard = backend_cell.lock().unwrap();
                                    let Some(backend) = guard.as_ref() else {
                                        return;
                                    };
                                    let my_id = backend.account().account_id_hex.clone();
                                    let my_label = my_avatar_label(backend, &my_id);
                                    let _ = with_inner_messages(&chats_messages, idx, |vm| {
                                        if let Some(pos) = find_message_row(vm, &temp_id) {
                                            vm.set_row_data(
                                                pos,
                                                pending_chat_message(&failed, &my_id, &my_label),
                                            );
                                        }
                                    });
                                }
                            }
                        }
                    });
                },
            );
        });
    }

    // Album send: all the images go out as ONE kind-9 message (multiple imeta
    // tags) so the confirmed bubble renders a grid. Optimistic pending bubble
    // shows the grid immediately from local previews; on ack we seed the
    // attachment cache (per image, under `real_id#index`) so the confirmed grid
    // shows the same pixels without a re-download, then swap the row. Mirrors
    // `spawn_attachment_send`'s reconcile, generalized to N images.
    // `replay_temp_id`: see `spawn_attachment_send` — `Some(id)` re-dispatches an
    // already-queued album from disk on reconnect.
    #[allow(clippy::too_many_arguments)]
    fn spawn_album_send(
        weak: slint::Weak<DarkMatterLinux>,
        backend_cell: Arc<Mutex<Option<Arc<Backend>>>>,
        group_ids: Arc<Mutex<Vec<String>>>,
        pending_state: Arc<Mutex<PendingState>>,
        vault_cell: Arc<Mutex<Option<Arc<Mutex<Vault>>>>>,
        group_hex: String,
        files: Vec<StagedFile>,
        replay_temp_id: Option<String>,
    ) {
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            let chats_messages = ui.get_chats_messages();
            let Some(idx) = group_ids
                .lock()
                .unwrap()
                .iter()
                .position(|g| g == &group_hex)
            else {
                return;
            };
            let guard = backend_cell.lock().unwrap();
            let Some(backend) = guard.as_ref() else {
                return;
            };

            let is_replay = replay_temp_id.is_some();
            let temp_id = replay_temp_id.unwrap_or_else(next_temp_id);
            let media: Vec<PendingMedia> = files
                .iter()
                .map(|f| PendingMedia {
                    file_name: f.file_name.clone(),
                    media_type: f.media_type.clone(),
                    size_bytes: f.bytes.len() as u64,
                    is_image: true,
                    is_video: false,
                    is_audio: false,
                    local_preview: f.preview.clone(),
                })
                .collect();
            let send = PendingSend {
                temp_id: temp_id.clone(),
                text: String::new(),
                failed: false,
                reply_to: None,
                media,
                effect: 0,
            };
            let already_present = pending_state
                .lock()
                .unwrap()
                .find_send(&group_hex, &temp_id)
                .is_some();
            if !already_present {
                pending_state
                    .lock()
                    .unwrap()
                    .add_send(&group_hex, send.clone());
                let my_id = backend.account().account_id_hex.clone();
                let my_label = my_avatar_label(backend, &my_id);
                let pending_row = pending_chat_message(&send, &my_id, &my_label);
                with_inner_messages(&chats_messages, idx, |vm| {
                    push_message_grouped(vm, pending_row)
                });
                ui.set_messages_scroll_tick(ui.get_messages_scroll_tick() + 1);
            }
            // Durably queue the album on first send (one entry, all images' bytes).
            if !is_replay {
                let my_id = backend.account().account_id_hex.clone();
                let queued_media: Vec<offline_queue::QueuedMedia> = files
                    .iter()
                    .map(|f| offline_queue::QueuedMedia {
                        file_name: f.file_name.clone(),
                        media_type: f.media_type.clone(),
                        bytes: f.bytes.clone(),
                        is_image: true,
                    })
                    .collect();
                offline_persist(
                    &vault_cell,
                    &offline_queue::QueuedSend {
                        temp_id: temp_id.clone(),
                        account_id_hex: my_id,
                        group_hex: group_hex.clone(),
                        kind: offline_queue::QueuedKind::Album(queued_media),
                        enqueued_at: offline_queue::now_secs(),
                    },
                );
            }
            offline_inflight_insert(&temp_id);

            // Previews (kept in image order) seed the cache under the real id on
            // ack; `items` carry the dim "WxH" so receivers lay out the grid.
            let previews: Vec<Option<PicturePixels>> =
                files.iter().map(|f| f.preview.clone()).collect();
            let items: Vec<(String, String, Vec<u8>, Option<String>)> = files
                .into_iter()
                .map(|f| {
                    let dim = f.preview.as_ref().map(|p| format!("{}x{}", p.w, p.h));
                    (f.file_name, f.media_type, f.bytes, dim)
                })
                .collect();

            let weak3 = weak.clone();
            let backend_cell3 = backend_cell.clone();
            let group_ids3 = group_ids.clone();
            let pending_state3 = pending_state.clone();
            let group_hex3 = group_hex.clone();
            let temp_id3 = temp_id.clone();
            backend.upload_album_async(&group_hex, items, move |result| {
                let weak = weak3.clone();
                let backend_cell = backend_cell3.clone();
                let group_ids = group_ids3.clone();
                let pending_state = pending_state3.clone();
                let group_hex = group_hex3.clone();
                let temp_id = temp_id3.clone();
                offline_inflight_remove(&temp_id);
                let (all, online): (Vec<AppMessageRecord>, bool) = if result.is_ok() {
                    let all = backend_cell
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|b| {
                            b.messages(&group_hex, Some(msg_window_for(&group_hex)))
                                .unwrap_or_default()
                        })
                        .unwrap_or_default();
                    (all, true)
                } else {
                    let online = backend_cell
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|b| b.relay_health().0 > 0)
                        .unwrap_or(false);
                    (Vec::new(), online)
                };
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    let chats_messages = ui.get_chats_messages();
                    let Some(idx) = group_ids
                        .lock()
                        .unwrap()
                        .iter()
                        .position(|g| g == &group_hex)
                    else {
                        return;
                    };
                    match result {
                        Ok(upload) => {
                            pending_state
                                .lock()
                                .unwrap()
                                .drop_send(&group_hex, &temp_id);
                            offline_queue::remove(&temp_id);
                            let guard = backend_cell.lock().unwrap();
                            let Some(backend) = guard.as_ref() else {
                                return;
                            };
                            let real_id = upload
                                .sent
                                .as_ref()
                                .and_then(|s| s.message_ids.first().cloned());
                            if let Some(id) = real_id.as_ref() {
                                for (i, p) in previews.iter().enumerate() {
                                    if let Some(px) = p {
                                        attachment_image_cache_put(att_key(id, i), px.clone());
                                    }
                                }
                            }
                            let confirmed_row: Option<ChatMessage> =
                                real_id.as_deref().and_then(|id| {
                                    let rec =
                                        all.iter().find(|m| m.message_id_hex == id).cloned()?;
                                    let overlay = pending_state.lock().unwrap();
                                    let my_id = backend.account().account_id_hex.clone();
                                    let my_label = my_avatar_label(backend, &my_id);
                                    Some(build_one_message_row(
                                        &rec, &all, &my_id, &my_label, &group_hex, &overlay,
                                        backend,
                                    ))
                                });
                            let swapped = with_inner_messages(&chats_messages, idx, |vm| {
                                let Some(pos) = find_message_row(vm, &temp_id) else {
                                    return false;
                                };
                                if let Some(mut row) = confirmed_row {
                                    preserve_grouping_flags(vm, pos, &mut row);
                                    vm.set_row_data(pos, row);
                                } else {
                                    vm.remove(pos);
                                }
                                true
                            });
                            if swapped != Some(true) {
                                let overlay = pending_state.lock().unwrap();
                                rebuild_chat_messages_from(
                                    backend,
                                    &overlay,
                                    &chats_messages,
                                    idx,
                                    &group_hex,
                                    &all,
                                );
                            }
                        }
                        Err(e) => {
                            eprintln!("[album] upload: {e:#}");
                            if !online {
                                eprintln!("[album] offline — left queued for flush");
                                return;
                            }
                            let mut overlay = pending_state.lock().unwrap();
                            overlay.mark_send_failed(&group_hex, &temp_id);
                            let failed = overlay.find_send(&group_hex, &temp_id);
                            drop(overlay);
                            if let Some(failed) = failed {
                                let guard = backend_cell.lock().unwrap();
                                let Some(backend) = guard.as_ref() else {
                                    return;
                                };
                                let my_id = backend.account().account_id_hex.clone();
                                let my_label = my_avatar_label(backend, &my_id);
                                let _ = with_inner_messages(&chats_messages, idx, |vm| {
                                    if let Some(pos) = find_message_row(vm, &temp_id) {
                                        vm.set_row_data(
                                            pos,
                                            pending_chat_message(&failed, &my_id, &my_label),
                                        );
                                    }
                                });
                            }
                        }
                    }
                });
            });
        });
    }

    // ─── Paste image (composer paste shortcut) ─────────────────────────
    //
    // The composer fires this on Ctrl/Cmd+V and Shift+Insert *in addition
    // to* the native text paste (which still runs). We probe the system
    // clipboard off-thread; image-intent content (an image target offered,
    // no plain-text target) is staged as an attachment chip — never
    // auto-sent.
    ui.on_paste_image({
        let weak = ui.as_weak();
        let staged_files = staged_files.clone();
        move || {
            let weak = weak.clone();
            let staged_files = staged_files.clone();
            // Throwaway thread, same rationale as `copy_to_clipboard_async`:
            // CLI helpers and arboard can block on the display server.
            std::thread::spawn(move || {
                let Some((bytes, media_type)) = paste_image_from_clipboard() else {
                    return;
                };
                let ext = media_type
                    .strip_prefix("image/")
                    .and_then(|s| s.split('+').next())
                    .unwrap_or("png")
                    .to_string();
                let file = staged_file_from_bytes(format!("pasted-image.{ext}"), media_type, bytes);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    let mut staged = staged_files.lock().unwrap();
                    staged.push(file);
                    refresh_staged_ui(&ui, &staged);
                });
            });
        }
    });

    // ─── Remove a staged attachment chip ───────────────────────────────
    ui.on_remove_staged({
        let weak = ui.as_weak();
        let staged_files = staged_files.clone();
        move |idx| {
            let Some(ui) = weak.upgrade() else { return };
            let mut staged = staged_files.lock().unwrap();
            let idx = idx as usize;
            if idx < staged.len() {
                staged.remove(idx);
            }
            refresh_staged_ui(&ui, &staged);
        }
    });

    // ─── Album cell tapped → open the slideshow at that image ──────────
    // The key is `message_id#index`. Pending album cells (temp ids start with
    // "pending:") aren't sent yet, so they don't open the viewer. Otherwise we
    // open the lightbox and let the slideshow builder load the tapped image
    // (cache hit → instant; miss → downloads) and wire up prev/next.
    ui.on_album_cell_clicked({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        move |key| {
            let Some(ui) = weak.upgrade() else { return };
            let key = key.to_string();
            if key.is_empty() || key.starts_with("pending:") {
                return;
            }
            let idx = ui.get_active_chat() as usize;
            let Some(group_hex) = group_ids.lock().unwrap().get(idx).cloned() else {
                return;
            };
            // Show the cached pixels immediately if we have them, else open on
            // the loading pill while the builder fetches the image.
            match attachment_image_cache_get(&key) {
                Some(px) => {
                    ui.set_image_viewer_image(image_from_pixels(&px));
                    ui.set_image_viewer_loading(false);
                }
                None => ui.set_image_viewer_loading(true),
            }
            ui.set_image_viewer_count(1);
            ui.set_image_viewer_index(1);
            ui.set_image_viewer_open(true);
            build_viewer_slideshow(
                ui.as_weak(),
                backend_cell.clone(),
                group_ids.clone(),
                group_hex,
                key,
            );
        }
    });

    // ─── Attachment clicked (download + open) ──────────────────────────
    //
    // Confirmed attachment bubble tapped. For images we decrypt + decode +
    // cache pixels then repaint the row so the preview swaps in. For other
    // files we prompt save-as first (so the user can cancel before any
    // network traffic) then write the decrypted bytes to that path.
    ui.on_attachment_clicked({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        let pending_state = pending_state.clone();
        let vault_cell = vault_cell.clone();
        move |message_id| {
            let Some(ui) = weak.upgrade() else { return };
            let mid = message_id.to_string();
            if mid.is_empty() || mid.starts_with("pending:") {
                return;
            }
            // Already decoded → tapping expands it into the full-window
            // lightbox instead of re-downloading. Also (re)build the slideshow
            // list so the chevrons can flip through the chat's other images.
            if let Some(img) = cached_attachment_image(&mid) {
                ui.set_image_viewer_image(img);
                ui.set_image_viewer_loading(false);
                ui.set_image_viewer_count(1);
                ui.set_image_viewer_index(1);
                ui.set_image_viewer_open(true);
                let idx = ui.get_active_chat() as usize;
                if let Some(group_hex) = group_ids.lock().unwrap().get(idx).cloned() {
                    build_viewer_slideshow(
                        ui.as_weak(),
                        backend_cell.clone(),
                        group_ids.clone(),
                        group_hex,
                        mid.clone(),
                    );
                }
                return;
            }
            {
                let mut set = match attachment_in_flight().lock() {
                    Ok(s) => s,
                    Err(_) => return,
                };
                if set.contains(&mid) {
                    return;
                }
                set.insert(mid.clone());
            }

            let idx = ui.get_active_chat() as usize;
            let Some(group_hex) = group_ids.lock().unwrap().get(idx).cloned() else {
                attachment_in_flight()
                    .lock()
                    .ok()
                    .map(|mut s| s.remove(&mid));
                return;
            };
            let Some(backend) = backend_cell.lock().unwrap().clone() else {
                attachment_in_flight()
                    .lock()
                    .ok()
                    .map(|mut s| s.remove(&mid));
                return;
            };
            // Unlocked vault for this session. Clones of this Arc ride into
            // the tokio tasks below to seal/unseal the disk cache.
            let vault = vault_cell.lock().unwrap().clone();
            // Resolving the tapped record means a sqlite read — do it on the
            // backend runtime, then hop back to the UI thread for the
            // in-flight row repaint and the download/cache dispatch (which
            // only spawns further async work).
            let weak = weak.clone();
            let backend_cell = backend_cell.clone();
            let group_ids = group_ids.clone();
            let pending_state = pending_state.clone();
            let b = backend.clone();
            backend.tokio_handle().spawn(async move {
                let all = b
                    .messages(&group_hex, Some(msg_window_for(&group_hex)))
                    .unwrap_or_default();
                let Some(rec) = all.iter().find(|m| m.message_id_hex == mid).cloned() else {
                    attachment_in_flight()
                        .lock()
                        .ok()
                        .map(|mut s| s.remove(&mid));
                    return;
                };
                let Some(reference) = parse_media_reference_from_tags(&rec.tags, rec.source_epoch)
                else {
                    attachment_in_flight()
                        .lock()
                        .ok()
                        .map(|mut s| s.remove(&mid));
                    return;
                };
                let is_image = mime_is_image(&reference.media_type);
                let is_video = mime_is_video(&reference.media_type);
                let is_audio = mime_is_audio(&reference.media_type);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    let chats_messages = ui.get_chats_messages();
                    {
                        let overlay = pending_state.lock().unwrap();
                        refresh_one_message_row_from(
                            &b,
                            &overlay,
                            &chats_messages,
                            idx,
                            &group_hex,
                            &mid,
                            &all,
                        );
                    }

                    // Audio → decrypt + play inline via rodio. No save dialog; the
                    // encrypted disk cache is read-through just like images/videos.
                    if is_audio {
                        attachment_in_flight()
                            .lock()
                            .ok()
                            .map(|mut s| s.remove(&mid));
                        let hash = reference.ciphertext_sha256.clone();
                        let b2 = b.clone();
                        let vault2 = vault.clone();
                        let weak2 = weak.clone();
                        let backend_cell2 = backend_cell.clone();
                        let group_ids2 = group_ids.clone();
                        let pending_state2 = pending_state.clone();
                        let group_hex2 = group_hex.clone();
                        let mid2 = mid.clone();
                        b.tokio_handle().spawn(async move {
                            if let Some(bytes) =
                                vault2.as_ref().and_then(|v| media_cache::get(v, &hash))
                            {
                                let _ = slint::invoke_from_event_loop(move || {
                                    start_audio_playback(
                                        weak2,
                                        backend_cell2,
                                        group_ids2,
                                        pending_state2,
                                        group_hex2,
                                        mid2,
                                        bytes,
                                    );
                                });
                                return;
                            }
                            let group_hex3 = group_hex2.clone();
                            b2.download_media_async(&group_hex2, reference, move |result| {
                                match result {
                                    Ok(dl) => {
                                        if let Some(v) = &vault2 {
                                            media_cache::put(v, &hash, &dl.plaintext);
                                        }
                                        let _ = slint::invoke_from_event_loop(move || {
                                            start_audio_playback(
                                                weak2,
                                                backend_cell2,
                                                group_ids2,
                                                pending_state2,
                                                group_hex3,
                                                mid2,
                                                dl.plaintext,
                                            );
                                        });
                                    }
                                    Err(e) => {
                                        eprintln!("[audio] download {mid2}: {e:#}");
                                    }
                                }
                            });
                        });
                        return;
                    }

                    // Video → open the in-app libmpv viewer and start playback. The
                    // poster (first frame) + duration get cached during playback, so
                    // the dismiss handler can repaint the bubble tile afterwards.
                    if is_video {
                        attachment_in_flight()
                            .lock()
                            .ok()
                            .map(|mut s| s.remove(&mid));
                        stop_current_player();
                        *current_video_duration().lock().unwrap() = 0.0;
                        *current_video_target().lock().unwrap() =
                            Some((group_hex.clone(), mid.clone()));
                        ui.set_video_viewer_has_frame(false);
                        ui.set_video_viewer_playing(false);
                        ui.set_video_viewer_progress(0.0);
                        ui.set_video_viewer_pos("0:00".into());
                        ui.set_video_viewer_dur("0:00".into());
                        ui.set_video_viewer_loading(true);
                        ui.set_video_viewer_open(true);
                        start_video_playback(
                            weak.clone(),
                            b.clone(),
                            group_hex.clone(),
                            mid.clone(),
                            reference.clone(),
                            vault.clone(),
                        );
                        return;
                    }

                    let tokio_handle = b.tokio_handle();

                    // After the (optional) save dialog resolves, kick off the actual
                    // download on the backend's tokio runtime.
                    let dispatch_download = {
                        let weak = weak.clone();
                        let backend_cell = backend_cell.clone();
                        let group_ids = group_ids.clone();
                        let pending_state = pending_state.clone();
                        let group_hex = group_hex.clone();
                        let mid = mid.clone();
                        let reference = reference.clone();
                        let vault = vault.clone();
                        move |target_path: Option<std::path::PathBuf>| {
                            let guard = backend_cell.lock().unwrap();
                            let Some(backend) = guard.as_ref() else {
                                attachment_in_flight()
                                    .lock()
                                    .ok()
                                    .map(|mut s| s.remove(&mid));
                                return;
                            };
                            let weak = weak.clone();
                            let backend_cell = backend_cell.clone();
                            let group_ids = group_ids.clone();
                            let pending_state = pending_state.clone();
                            let group_hex = group_hex.clone();
                            let mid = mid.clone();
                            let group_hex_inner = group_hex.clone();
                            let vault = vault.clone();
                            let cache_hash = reference.ciphertext_sha256.clone();
                            backend.download_media_async(
                                &group_hex,
                                reference.clone(),
                                move |result| {
                                    let weak = weak.clone();
                                    let backend_cell = backend_cell.clone();
                                    let group_ids = group_ids.clone();
                                    let pending_state = pending_state.clone();
                                    let group_hex = group_hex_inner.clone();
                                    let mid = mid.clone();
                                    match result {
                                        Ok(dl) => {
                                            if is_image {
                                                // Persist the decrypted original bytes to
                                                // the encrypted disk cache so this image
                                                // survives a restart without another
                                                // Blossom round-trip + decrypt.
                                                if let Some(v) = &vault {
                                                    media_cache::put(v, &cache_hash, &dl.plaintext);
                                                }
                                                match image::load_from_memory(&dl.plaintext) {
                                                    Ok(img) => {
                                                        let rgba = img.to_rgba8();
                                                        let pixels = PicturePixels {
                                                            w: rgba.width(),
                                                            h: rgba.height(),
                                                            rgba: rgba.into_raw(),
                                                        };
                                                        attachment_image_cache_put(
                                                            mid.clone(),
                                                            pixels,
                                                        );
                                                    }
                                                    Err(e) => {
                                                        eprintln!("[attach] decode {mid}: {e:#}")
                                                    }
                                                }
                                            } else if let Some(path) = &target_path
                                                && let Err(e) = std::fs::write(path, &dl.plaintext)
                                            {
                                                eprintln!(
                                                    "[attach] write {}: {e:#}",
                                                    path.display()
                                                );
                                            }
                                        }
                                        Err(e) => eprintln!("[attach] download {mid}: {e:#}"),
                                    }
                                    // This completion already runs on the backend
                                    // runtime; the async refresh keeps the snapshot
                                    // read off the UI thread.
                                    attachment_in_flight()
                                        .lock()
                                        .ok()
                                        .map(|mut s| s.remove(&mid));
                                    let Some(backend) = backend_cell.lock().unwrap().clone() else {
                                        return;
                                    };
                                    refresh_one_message_row_async(
                                        &backend,
                                        weak,
                                        pending_state,
                                        group_ids,
                                        group_hex,
                                        mid,
                                    );
                                },
                            );
                        }
                    };

                    if is_image {
                        // Read-through the encrypted disk cache before paying for a
                        // network round-trip. On a hit we decrypt + decode locally and
                        // repaint the row; on a miss we fall back to the live download
                        // (which write-throughs the cache for next time).
                        match vault.clone() {
                            Some(vault) => {
                                let hash = reference.ciphertext_sha256.clone();
                                let weak = weak.clone();
                                let backend_cell = backend_cell.clone();
                                let group_ids = group_ids.clone();
                                let pending_state = pending_state.clone();
                                let group_hex = group_hex.clone();
                                let mid = mid.clone();
                                tokio_handle.spawn(async move {
                                    let hit = media_cache::get(&vault, &hash).and_then(|plain| {
                                        image::load_from_memory(&plain).ok().map(|img| {
                                            let rgba = img.to_rgba8();
                                            PicturePixels {
                                                w: rgba.width(),
                                                h: rgba.height(),
                                                rgba: rgba.into_raw(),
                                            }
                                        })
                                    });
                                    match hit {
                                        Some(pixels) => {
                                            // Already on the backend runtime; both
                                            // caches are plain process-wide mutexes,
                                            // so no event-loop hop is needed before
                                            // the async row refresh.
                                            attachment_image_cache_put(mid.clone(), pixels);
                                            attachment_in_flight()
                                                .lock()
                                                .ok()
                                                .map(|mut s| s.remove(&mid));
                                            let Some(backend) =
                                                backend_cell.lock().unwrap().clone()
                                            else {
                                                return;
                                            };
                                            refresh_one_message_row_async(
                                                &backend,
                                                weak,
                                                pending_state,
                                                group_ids,
                                                group_hex,
                                                mid,
                                            );
                                        }
                                        None => dispatch_download(None),
                                    }
                                });
                            }
                            None => dispatch_download(None),
                        }
                    } else {
                        let default_name = reference.file_name.clone();
                        let weak_clear = weak.clone();
                        let group_ids_clear = group_ids.clone();
                        let backend_cell_clear = backend_cell.clone();
                        let pending_state_clear = pending_state.clone();
                        let group_hex_clear = group_hex.clone();
                        let mid_clear = mid.clone();
                        tokio_handle.spawn(async move {
                            let chosen = tokio::task::spawn_blocking(move || {
                                rfd::FileDialog::new()
                                    .set_title("Save attachment")
                                    .set_file_name(&default_name)
                                    .save_file()
                            })
                            .await
                            .ok()
                            .flatten();
                            let _ = slint::invoke_from_event_loop(move || match chosen {
                                Some(path) => dispatch_download(Some(path)),
                                None => {
                                    attachment_in_flight()
                                        .lock()
                                        .ok()
                                        .map(|mut s| s.remove(&mid_clear));
                                    let Some(backend) = backend_cell_clear.lock().unwrap().clone()
                                    else {
                                        return;
                                    };
                                    refresh_one_message_row_async(
                                        &backend,
                                        weak_clear,
                                        pending_state_clear,
                                        group_ids_clear,
                                        group_hex_clear,
                                        mid_clear,
                                    );
                                }
                            });
                        });
                    }
                }); // end invoke_from_event_loop (UI-thread dispatch)
            }); // end backend-runtime record resolution
        }
    });

    // ─── Audio play / seek (inline voice-message player) ───────────────
    //
    // The bubble's audio player routes play/pause and progress-bar taps here.
    // Play toggles the current clip; seek jumps to a fraction of the duration.
    // Both operate on the per-message encrypted cache just like images/videos.
    ui.on_audio_play_clicked({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        let pending_state = pending_state.clone();
        let vault_cell = vault_cell.clone();
        move |message_id| {
            let Some(ui) = weak.upgrade() else { return };
            let mid = message_id.to_string();
            if mid.is_empty() || mid.starts_with("pending:") {
                return;
            }
            // Toggle if this message is already the active player.
            let is_current = current_audio_message_id()
                .lock()
                .unwrap()
                .as_ref()
                .map(|id| id == &mid)
                .unwrap_or(false);
            if is_current {
                with_active_player(|p| {
                    if let Some(player) = p.as_ref() {
                        player.toggle();
                    }
                });
                return;
            }

            let idx = ui.get_active_chat() as usize;
            let Some(group_hex) = group_ids.lock().unwrap().get(idx).cloned() else {
                return;
            };
            let Some(backend) = backend_cell.lock().unwrap().clone() else {
                return;
            };
            let vault = vault_cell.lock().unwrap().clone();
            let weak2 = weak.clone();
            let backend_cell2 = backend_cell.clone();
            let group_ids2 = group_ids.clone();
            let pending_state2 = pending_state.clone();
            let b = backend.clone();
            backend.tokio_handle().spawn(async move {
                let all = b
                    .messages(&group_hex, Some(msg_window_for(&group_hex)))
                    .unwrap_or_default();
                let Some(rec) = all.iter().find(|m| m.message_id_hex == mid).cloned() else {
                    return;
                };
                let Some(reference) = parse_media_reference_from_tags(&rec.tags, rec.source_epoch)
                else {
                    return;
                };
                let hash = reference.ciphertext_sha256.clone();
                if let Some(bytes) = vault.as_ref().and_then(|v| media_cache::get(v, &hash)) {
                    let weak3 = weak2.clone();
                    let backend_cell3 = backend_cell2.clone();
                    let group_ids3 = group_ids2.clone();
                    let pending_state3 = pending_state2.clone();
                    let group_hex3 = group_hex.clone();
                    let mid3 = mid.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        start_audio_playback(
                            weak3,
                            backend_cell3,
                            group_ids3,
                            pending_state3,
                            group_hex3,
                            mid3,
                            bytes,
                        );
                    });
                    return;
                }
                let weak4 = weak2.clone();
                let backend_cell4 = backend_cell2.clone();
                let group_ids4 = group_ids2.clone();
                let pending_state4 = pending_state2.clone();
                let group_hex4 = group_hex.clone();
                let mid4 = mid.clone();
                let vault4 = vault.clone();
                let hash4 = hash.clone();
                b.download_media_async(&group_hex, reference, move |result| match result {
                    Ok(dl) => {
                        if let Some(v) = &vault4 {
                            media_cache::put(v, &hash4, &dl.plaintext);
                        }
                        let _ = slint::invoke_from_event_loop(move || {
                            start_audio_playback(
                                weak4,
                                backend_cell4,
                                group_ids4,
                                pending_state4,
                                group_hex4,
                                mid4,
                                dl.plaintext,
                            );
                        });
                    }
                    Err(e) => eprintln!("[audio] download {mid}: {e:#}"),
                });
            });
        }
    });

    ui.on_audio_seek_clicked({
        move |message_id, fraction| {
            let mid = message_id.to_string();
            let is_current = current_audio_message_id()
                .lock()
                .unwrap()
                .as_ref()
                .map(|id| id == &mid)
                .unwrap_or(false);
            if is_current {
                with_active_player(|p| {
                    if let Some(player) = p.as_ref() {
                        let dur = player.state().duration;
                        player.seek(fraction as f64 * dur);
                    }
                });
            }
        }
    });

    // ─── Voice message recording (composer mic) ────────────────────────
    ui.on_record_clicked({
        let weak = ui.as_weak();
        move || {
            let recorder = match audio::AudioRecorder::start() {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("[audio] start recording: {e:#}");
                    return;
                }
            };
            with_active_recorder(|r| {
                *r = Some(recorder);
            });
            *recording_start().lock().unwrap() = Some(std::time::Instant::now());
            let weak_t = weak.clone();
            std::thread::spawn(move || {
                for secs in 1.. {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    let still_recording = recording_start().lock().unwrap().is_some();
                    if !still_recording {
                        break;
                    }
                    let _ = slint::invoke_from_event_loop({
                        let weak = weak_t.clone();
                        move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.set_composer_recording_secs(secs);
                            }
                        }
                    });
                    // Auto-stop at the maximum clip length.
                    if secs >= 120 {
                        let _ = slint::invoke_from_event_loop({
                            let weak = weak_t.clone();
                            move || {
                                if let Some(ui) = weak.upgrade() {
                                    ui.invoke_stop_recording();
                                }
                            }
                        });
                        break;
                    }
                }
            });
            let weak_i = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_i.upgrade() {
                    ui.set_composer_recording(true);
                    ui.set_composer_recording_secs(0);
                }
            });
        }
    });

    ui.on_stop_recording({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        let pending_state = pending_state.clone();
        let vault_cell = vault_cell.clone();
        move || {
            let recorder = with_active_recorder(|r| r.take());
            let Some(recorder) = recorder else { return };
            *recording_start().lock().unwrap() = None;

            // Stop/encode on the UI thread because the cpal Stream is !Send.
            // Encoding a short WAV clip is fast, so this keeps the code simple.
            let bytes = match recorder.stop() {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[audio] stop recording: {e:#}");
                    if let Some(ui) = weak.upgrade() {
                        ui.set_composer_recording(false);
                    }
                    return;
                }
            };
            if let Some(ui) = weak.upgrade() {
                ui.set_composer_recording(false);
            }
            let Some(ui) = weak.upgrade() else { return };
            let idx = ui.get_active_chat() as usize;
            let Some(group_hex) = group_ids.lock().unwrap().get(idx).cloned() else {
                return;
            };
            let guard = backend_cell.lock().unwrap();
            let Some(_backend) = guard.as_ref() else {
                return;
            };
            drop(guard);
            spawn_attachment_send(
                weak.clone(),
                backend_cell.clone(),
                group_ids.clone(),
                pending_state.clone(),
                vault_cell.clone(),
                group_hex,
                "voice-message.wav".to_string(),
                "audio/wav".to_string(),
                bytes,
                false,
                None,
                None,
            );
        }
    });

    ui.on_cancel_recording({
        let weak = ui.as_weak();
        move || {
            with_active_recorder(|r| {
                *r = None;
            });
            *recording_start().lock().unwrap() = None;
            let weak_i = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak_i.upgrade() {
                    ui.set_composer_recording(false);
                }
            });
        }
    });

    // ─── Reply target (set / cancel) ───────────────────────────────────
    //
    // The bubble's "↩" affordance fires `request-reply(id, preview, author)`.
    // We stash all three on the root so the composer chip renders, then the
    // next send pulls them off and routes through `reply_text_async`.
    ui.on_request_reply({
        let weak = ui.as_weak();
        move |message_id, preview, author| {
            let Some(ui) = weak.upgrade() else { return };
            let trimmed = truncate_preview(preview.as_str(), 160);
            ui.set_reply_target_id(message_id);
            ui.set_reply_target_author(author);
            ui.set_reply_target_preview(s(&trimmed));
        }
    });
    ui.on_cancel_reply({
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_reply_target_id(s(""));
            ui.set_reply_target_author(s(""));
            ui.set_reply_target_preview(s(""));
        }
    });

    // ─── Edit target (enter / cancel) ──────────────────────────────────
    //
    // The bubble's edit affordance (own messages only) fires
    // `request-edit(id, current_text)`. We load the current text into the
    // composer and stash the target id; the next send routes through
    // `edit_op`. Entering edit mode clears any pending reply target.
    ui.on_request_edit({
        let weak = ui.as_weak();
        move |message_id, current_text| {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_reply_target_id(s(""));
            ui.set_reply_target_author(s(""));
            ui.set_reply_target_preview(s(""));
            ui.set_editing_message_id(message_id);
            ui.set_composer_draft(current_text);
        }
    });
    ui.on_cancel_edit({
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_editing_message_id(s(""));
            ui.set_composer_draft(s(""));
        }
    });

    // ─── Edit history (visible to anyone) ──────────────────────────────
    //
    // Tapping a bubble's "(edited)" label asks Rust to assemble the full
    // version list (original + each author-authored kind-1009) and open the
    // modal. Empty history (race) just no-ops.
    ui.on_show_edit_history({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        move |message_id| {
            let Some(ui) = weak.upgrade() else { return };
            let idx = ui.get_active_chat() as usize;
            let Some(group_hex) = group_ids.lock().unwrap().get(idx).cloned() else {
                return;
            };
            let Some(backend) = backend_cell.lock().unwrap().clone() else {
                return;
            };
            // Window read on the backend runtime; the modal opens a beat
            // later instead of stalling the UI thread on sqlite.
            let weak = ui.as_weak();
            let message_id = message_id.to_string();
            let b = backend.clone();
            backend.tokio_handle().spawn(async move {
                let all = b
                    .messages(&group_hex, Some(msg_window_for(&group_hex)))
                    .unwrap_or_default();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    let versions = build_edit_history(&all, &message_id);
                    if versions.is_empty() {
                        return;
                    }
                    ui.set_edit_history(ModelRc::new(VecModel::from(versions)));
                    ui.set_edit_history_open(true);
                });
            });
        }
    });
    ui.on_dismiss_edit_history({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_edit_history_open(false);
            }
        }
    });

    ui.on_dismiss_image_viewer({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_image_viewer_open(false);
                ui.set_image_viewer_loading(false);
            }
            VIEWER_SLIDESHOW.with(|s| *s.borrow_mut() = ViewerSlideshow::default());
        }
    });

    // ─── In-app video viewer ───────────────────────────────────────────
    // Dropping the player joins its render/event threads and frees the mpv
    // handle (stopping audio). The first-frame poster + duration captured
    // during playback are now cached, so repaint that bubble's tile.
    ui.on_dismiss_video_viewer({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let pending_state = pending_state.clone();
        let group_ids = group_ids.clone();
        move || {
            use std::sync::atomic::Ordering;
            stop_current_player();
            if let Some(ui) = weak.upgrade() {
                // Never leave the whole app stuck fullscreen after closing.
                if video_fullscreen().swap(false, Ordering::AcqRel) {
                    ui.window().set_fullscreen(false);
                }
                ui.set_video_viewer_open(false);
                ui.set_video_viewer_loading(false);
                ui.set_video_viewer_has_frame(false);
                ui.set_video_viewer_playing(false);
                ui.set_video_viewer_frame(slint::Image::default());
            }
            let target = current_video_target().lock().ok().and_then(|t| t.clone());
            if let Some((group_hex, mid)) = target
                && let Some(backend) = backend_cell.lock().unwrap().clone()
            {
                refresh_one_message_row_async(
                    &backend,
                    weak.clone(),
                    pending_state.clone(),
                    group_ids.clone(),
                    group_hex,
                    mid,
                );
            }
            *current_video_target().lock().unwrap() = None;
        }
    });

    ui.on_video_viewer_toggle_play({
        let weak = ui.as_weak();
        move || {
            if let Some(player) = current_player().lock().unwrap().as_ref() {
                let now_playing = !player.toggle_pause();
                if let Some(ui) = weak.upgrade() {
                    ui.set_video_viewer_playing(now_playing);
                }
            }
        }
    });

    ui.on_video_viewer_seek(move |fraction| {
        let dur = *current_video_duration().lock().unwrap();
        if dur > 0.0
            && let Some(player) = current_player().lock().unwrap().as_ref()
        {
            player.seek((fraction as f64).clamp(0.0, 1.0) * dur);
        }
    });

    ui.on_video_viewer_seek_relative(move |secs| {
        if let Some(player) = current_player().lock().unwrap().as_ref() {
            player.seek_relative(secs as f64);
        }
    });

    ui.on_video_viewer_fullscreen({
        let weak = ui.as_weak();
        move || {
            use std::sync::atomic::Ordering;
            let want = !video_fullscreen().load(Ordering::Acquire);
            video_fullscreen().store(want, Ordering::Release);
            if let Some(ui) = weak.upgrade() {
                ui.window().set_fullscreen(want);
            }
        }
    });

    // ─── Lightbox slideshow nav ────────────────────────────────────────
    // Step the position and load that image (cache hit → instant; miss →
    // download with the loading pill up). `prev`/`next` are no-ops at the
    // ends — the UI hides the chevron there, but a stray ←/→ key is harmless.
    ui.on_image_viewer_prev({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let target = VIEWER_SLIDESHOW.with(|s| {
                let mut s = s.borrow_mut();
                if s.pos > 0 {
                    s.pos -= 1;
                }
                s.items.get(s.pos).map(|it| (s.pos, it.clone()))
            });
            if let Some((pos, item)) = target {
                ui.set_image_viewer_index((pos + 1) as i32);
                load_viewer_image(&ui, &backend_cell, &group_ids, pos, item);
            }
        }
    });
    ui.on_image_viewer_next({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let target = VIEWER_SLIDESHOW.with(|s| {
                let mut s = s.borrow_mut();
                if s.pos + 1 < s.items.len() {
                    s.pos += 1;
                }
                s.items.get(s.pos).map(|it| (s.pos, it.clone()))
            });
            if let Some((pos, item)) = target {
                ui.set_image_viewer_index((pos + 1) as i32);
                load_viewer_image(&ui, &backend_cell, &group_ids, pos, item);
            }
        }
    });

    // ─── Emoji picker ─────────────────────────────────────────────────
    // The picker's source list is the entire Unicode emoji catalog from the
    // `emojis` crate, filtered by the search query. Rebuilt on each query
    // change and on open.
    let emoji_query: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    let refresh_emoji_rows = {
        let weak = ui.as_weak();
        let emoji_query = emoji_query.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let q = emoji_query.borrow().clone();
            let list = build_emoji_list(&q);
            let total = list.len();
            ui.set_emoji_list(ModelRc::new(VecModel::from(list)));
            ui.set_emoji_shown(total as i32);
        }
    };

    ui.on_emoji_picker_requested({
        let weak = ui.as_weak();
        let emoji_query = emoji_query.clone();
        let refresh = refresh_emoji_rows.clone();
        move |message_id, anchor_x, anchor_y| {
            let Some(ui) = weak.upgrade() else { return };
            *emoji_query.borrow_mut() = String::new();
            ui.set_emoji_query(s(""));
            ui.set_emoji_target_message_id(message_id);
            ui.set_emoji_anchor_x(anchor_x);
            ui.set_emoji_anchor_y(anchor_y);
            refresh();
            ui.set_show_emoji_picker(true);
        }
    });

    ui.on_emoji_query_changed({
        let emoji_query = emoji_query.clone();
        let refresh = refresh_emoji_rows.clone();
        move |q| {
            *emoji_query.borrow_mut() = q.to_string();
            refresh();
        }
    });

    ui.on_emoji_picker_dismissed({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_show_emoji_picker(false);
            }
        }
    });

    ui.on_emoji_picked({
        let weak = ui.as_weak();
        move |message_id, emoji| {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_show_emoji_picker(false);
            // Sentinel target: append to the composer draft instead of
            // reacting to a message.
            if message_id == "\u{1}composer" {
                let mut draft = ui.get_composer_draft().to_string();
                draft.push_str(emoji.as_str());
                ui.set_composer_draft(draft.into());
                return;
            }
            ui.invoke_react_message(message_id, emoji);
        }
    });

    // ─── Mention autocomplete (@npub) ─────────────────────────────────
    // As the user types we look back from the caret for an active `@token`; if
    // one is present we filter the open chat's members into a popup. Choosing a
    // member splices `@<npub> ` over the token. `mention_span` carries the byte
    // span [at, caret) of the token from a keystroke to its commit.
    let mention_span: Rc<RefCell<Option<(usize, usize)>>> = Rc::new(RefCell::new(None));

    ui.on_composer_input_changed({
        let weak = ui.as_weak();
        let mention_span = mention_span.clone();
        move |cursor| {
            let Some(ui) = weak.upgrade() else { return };
            let draft = ui.get_composer_draft().to_string();
            let cursor = (cursor.max(0) as usize).min(draft.len());
            match detect_mention(&draft, cursor) {
                Some((at, query)) => {
                    let cands = filter_mention_candidates(&ui, &query);
                    if cands.is_empty() {
                        *mention_span.borrow_mut() = None;
                        ui.set_mention_active(false);
                        return;
                    }
                    *mention_span.borrow_mut() = Some((at, cursor));
                    ui.set_mention_candidates(model(cands));
                    ui.set_mention_selected(0);
                    ui.set_mention_active(true);
                }
                None => {
                    *mention_span.borrow_mut() = None;
                    ui.set_mention_active(false);
                }
            }
        }
    });

    ui.on_mention_nav({
        let weak = ui.as_weak();
        move |delta| {
            let Some(ui) = weak.upgrade() else { return };
            let n = ui.get_mention_candidates().row_count() as i32;
            if n == 0 {
                return;
            }
            let sel = (ui.get_mention_selected() + delta).rem_euclid(n);
            ui.set_mention_selected(sel);
        }
    });

    ui.on_mention_commit({
        let weak = ui.as_weak();
        let mention_span = mention_span.clone();
        move || {
            if let Some(ui) = weak.upgrade() {
                let sel = ui.get_mention_selected();
                commit_mention(&ui, &mention_span, sel);
            }
        }
    });

    ui.on_mention_choose({
        let weak = ui.as_weak();
        let mention_span = mention_span.clone();
        move |index| {
            if let Some(ui) = weak.upgrade() {
                commit_mention(&ui, &mention_span, index);
            }
        }
    });

    ui.on_mention_dismiss({
        let weak = ui.as_weak();
        let mention_span = mention_span.clone();
        move || {
            if let Some(ui) = weak.upgrade() {
                *mention_span.borrow_mut() = None;
                ui.set_mention_active(false);
            }
        }
    });

    // ─── Reactions (optimistic, surgical) ─────────────────────────────
    //
    // Stamp the overlay locally, refresh ONLY the target row, dispatch the
    // kind-7 in the background, then refresh ONLY the target row again on
    // ack. No siblings are remounted; the bubble's enter animation never
    // re-fires on neighbours.
    let react_op = {
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        let pending_state = pending_state.clone();
        let weak = ui.as_weak();
        Rc::new(move |op: PendingReactionOp, target: String| {
            let Some(ui) = weak.upgrade() else { return };
            let idx = ui.get_active_chat() as usize;
            let Some(group_hex) = group_ids.lock().unwrap().get(idx).cloned() else {
                return;
            };
            let chats_messages = ui.get_chats_messages();

            // 1. Optimistic overlay + model-only row mutation. No DB read
            //    on this path — the chip just appears where it should.
            {
                let mut overlay = pending_state.lock().unwrap();
                overlay
                    .reactions
                    .insert((group_hex.clone(), target.clone()), op.clone());
            }
            apply_reaction_to_model_row(&chats_messages, idx, &target, &op);

            // 2. Dispatch + reconcile (also surgical).
            let guard = backend_cell.lock().unwrap();
            let Some(backend) = guard.as_ref() else {
                return;
            };
            let weak_cb = weak.clone();
            let group_ids_cb = group_ids.clone();
            let pending_state_cb = pending_state.clone();
            let backend_cell_cb = backend_cell.clone();
            let group_hex_cb = group_hex.clone();
            let target_cb = target.clone();
            let label = match &op {
                PendingReactionOp::Add(_) => "react",
                PendingReactionOp::Remove => "unreact",
            };
            let on_done = move |result: anyhow::Result<marmot_app::SendSummary>| {
                let weak = weak_cb.clone();
                let group_ids = group_ids_cb.clone();
                let pending_state = pending_state_cb.clone();
                let backend_cell = backend_cell_cb.clone();
                let group_hex = group_hex_cb.clone();
                let target = target_cb.clone();
                let label = label;
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    {
                        let mut overlay = pending_state.lock().unwrap();
                        if let Err(e) = &result {
                            eprintln!("[{label}] {e:#}");
                            ui.set_backend_error(friendly_error(label, e).into());
                        }
                        overlay
                            .reactions
                            .remove(&(group_hex.clone(), target.clone()));
                    }
                    let Some(backend) = backend_cell.lock().unwrap().clone() else {
                        return;
                    };
                    // Snapshot read + row rebuild ride the backend runtime —
                    // no sqlite on the UI thread.
                    refresh_one_message_row_async(
                        &backend,
                        ui.as_weak(),
                        pending_state.clone(),
                        group_ids.clone(),
                        group_hex,
                        target,
                    );
                });
            };
            match op {
                PendingReactionOp::Add(emoji) => {
                    backend.react_async(&group_hex, &target, &emoji, on_done);
                }
                PendingReactionOp::Remove => {
                    backend.unreact_async(&group_hex, &target, on_done);
                }
            }
        })
    };

    ui.on_react_message({
        let react_op = react_op.clone();
        move |message_id, emoji| {
            if message_id.as_str().starts_with("pending:") {
                return;
            }
            react_op(
                PendingReactionOp::Add(emoji.to_string()),
                message_id.to_string(),
            );
        }
    });

    ui.on_unreact_message({
        let react_op = react_op.clone();
        move |message_id| {
            if message_id.as_str().starts_with("pending:") {
                return;
            }
            react_op(PendingReactionOp::Remove, message_id.to_string());
        }
    });

    // ─── Edit profile ──────────────────────────────────────────────────
    ui.on_start_edit_profile({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_profile_status(s(""));
                ui.set_profile_editing(true);
            }
        }
    });

    ui.on_cancel_edit_profile({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            if let Some(b) = backend_cell.lock().unwrap().as_ref() {
                populate_profile_async(&ui, b);
            }
            ui.set_profile_status(s(""));
            ui.set_profile_editing(false);
        }
    });

    ui.on_save_profile({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(backend) = backend_cell.lock().unwrap().clone() else {
                // Backend failed to boot earlier. Show the captured reason
                // instead of a generic message so the user can act on it.
                let saved = ui.get_backend_error().to_string();
                let msg = if saved.is_empty() {
                    "backend not ready (no boot error captured — check stderr)".to_string()
                } else {
                    format!("backend not ready: {saved}")
                };
                ui.set_profile_status(msg.into());
                return;
            };
            let profile = profile_from_ui(&ui);
            ui.set_profile_busy(true);
            ui.set_profile_status(s("publishing…"));
            // Publishing the kind-0 is a relay round-trip — worker thread, so
            // "publishing…" actually shows instead of freezing the window.
            let weak = weak.clone();
            std::thread::spawn(move || {
                let result = backend.save_profile(profile);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    ui.set_profile_busy(false);
                    match result {
                        Ok(saved) => {
                            apply_profile(&ui, Some(&saved));
                            ui.set_profile_editing(false);
                            ui.set_profile_status(s("profile published"));
                        }
                        Err(e) => {
                            eprintln!("[profile] save failed: {e:#}");
                            ui.set_profile_status(friendly_error("save profile", &e).into());
                        }
                    }
                });
            });
        }
    });

    // ─── Upload profile picture to Blossom ─────────────────────────────
    //
    // Pick a local image, upload the raw bytes to Blossom as a *public* blob,
    // and on success drop the returned URL into the picture field + refresh the
    // avatar preview. The rfd dialog runs on a blocking task (its xdg-portal
    // backend drives ashpd/zbus); everything that touches the UI bounces back
    // through `invoke_from_event_loop`.
    ui.on_upload_profile_picture({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_profile_uploading() {
                return;
            }
            let tokio_handle = {
                let guard = backend_cell.lock().unwrap();
                match guard.as_ref() {
                    Some(b) => b.tokio_handle(),
                    None => {
                        ui.set_profile_status(s("backend not ready"));
                        return;
                    }
                }
            };
            ui.set_profile_uploading(true);
            ui.set_profile_status(s("choosing image…"));
            let weak = ui.as_weak();
            let backend_cell = backend_cell.clone();
            tokio_handle.spawn(async move {
                let chosen = tokio::task::spawn_blocking(|| {
                    rfd::FileDialog::new()
                        .set_title("Choose a profile picture")
                        .add_filter("Images", &["png", "jpg", "jpeg", "gif", "webp"])
                        .pick_file()
                })
                .await
                .ok()
                .flatten();

                let Some(path) = chosen else {
                    // Cancelled — reset state on the UI thread.
                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.set_profile_uploading(false);
                            ui.set_profile_status(s(""));
                        }
                    });
                    return;
                };

                let bytes = match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(e) => {
                        let msg = format!("could not read file: {e}");
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.set_profile_uploading(false);
                                ui.set_profile_status(msg.into());
                            }
                        });
                        return;
                    }
                };
                let content_type = mime_guess::from_path(&path)
                    .first_or_octet_stream()
                    .essence_str()
                    .to_string();

                // Tell the user we're uploading now (file picked).
                {
                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.set_profile_status(s("uploading to Blossom…"));
                        }
                    });
                }

                // Hand the upload to the backend (it signs with the account
                // keys). The callback fires on a tokio worker; hop back to the
                // event loop for all UI work.
                let weak_done = weak.clone();
                let backend_cell_done = backend_cell.clone();
                let guard = backend_cell.lock().unwrap();
                let Some(backend) = guard.as_ref() else {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak_done.upgrade() {
                            ui.set_profile_uploading(false);
                            ui.set_profile_status(s("backend not ready"));
                        }
                    });
                    return;
                };
                backend.upload_public_blob_async(bytes, content_type, move |result| {
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = weak_done.upgrade() else {
                            return;
                        };
                        ui.set_profile_uploading(false);
                        match result {
                            Ok(url) => {
                                ui.set_profile_picture(url.clone().into());
                                ui.set_profile_status(s("picture uploaded — Save to publish"));
                                // Refresh the avatar preview from the new URL.
                                if let Some(backend) = backend_cell_done.lock().unwrap().as_ref() {
                                    fetch_profile_picture(&ui, backend, &url);
                                }
                            }
                            Err(e) => {
                                eprintln!("[profile] picture upload failed: {e:#}");
                                ui.set_profile_status(friendly_error("upload picture", &e).into());
                            }
                        }
                    });
                });
            });
        }
    });

    // One-time emoji setup:
    //   1. Decode the build-time sprite sheet PNG into a slint::Image and
    //      hand it to the picker.
    //   2. Populate `emoji-rows` so the grid has clip positions ready.
    ui.set_emoji_sprite(emoji_sprite_image());
    ui.set_emoji_tile(emoji_sprite_map::TILE as i32);
    // Also populate the `EmojiSheet` global so deeply-nested components
    // (chat bubbles in particular) can render inline emoji without having
    // the sprite plumbed through every intermediate row.
    let sheet = ui.global::<EmojiSheet>();
    sheet.set_sprite(emoji_sprite_image());
    sheet.set_tile(emoji_sprite_map::TILE as i32);
    // Message-effect catalog for the composer's send-button picker. Resolve each
    // effect's emoji to its sprite tile; drop any the sheet doesn't carry.
    {
        let choices: Vec<EffectChoice> = EFFECTS
            .iter()
            .filter_map(|(id, _, _)| {
                effect_clip(*id).map(|(x, y)| EffectChoice {
                    id: *id,
                    clip_x: x as i32,
                    clip_y: y as i32,
                })
            })
            .collect();
        ui.global::<EffectCatalog>()
            .set_choices(ModelRc::new(VecModel::from(choices)));
    }
    refresh_emoji_rows();

    // Markdown links/anchors in chat bubbles activate through this global so
    // they don't have to be plumbed through every row component. nostr: profile
    // references (@mentions render as `nostr:npub…` anchors) open the in-app
    // profile modal; everything else goes to the platform handler (xdg-open).
    ui.global::<Linkout>().on_open({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        move |url| {
            let url = url.as_str();
            if let Some(reference) = url.strip_prefix("nostr:")
                && let Some(hex) = nostr_ref_to_hex(reference)
                && let Some(ui) = weak.upgrade()
            {
                open_profile_modal(&ui, &backend_cell, &hex);
                return;
            }
            open_external(url);
        }
    });

    // Avatar / sender-name taps anywhere in the message tree (and the members
    // panel) land here with the account-id hex.
    ui.global::<ProfileSink>().on_open({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        move |account_id| {
            let Some(ui) = weak.upgrade() else { return };
            open_profile_modal(&ui, &backend_cell, account_id.as_str());
        }
    });

    ui.on_peer_profile_dismissed({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_peer_profile_open(false);
            }
        }
    });

    // Chat-list stamps are date-granular ("Yesterday", weekday, …), so they
    // only go stale when the civil date flips. A cheap minute tick watches
    // for midnight and rebuilds the chat models once per day-change. Held in
    // a binding so the timer lives until `run()` returns.
    let _stamp_timer = slint::Timer::default();
    {
        let refresh = refresh_all_chat_models.clone();
        let day = std::cell::Cell::new(jiff::Zoned::now().date());
        _stamp_timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_secs(60),
            move || {
                let today = jiff::Zoned::now().date();
                if day.get() != today {
                    day.set(today);
                    refresh();
                }
            },
        );
    }

    // ─── Durable offline send queue: flush + reconnect watcher ─────────────
    //
    // `flush_offline_queue` reconciles the encrypted on-disk queue with the UI:
    // it renders a pending bubble for every queued send that isn't on screen yet
    // (so messages composed offline are visible across restarts), and — when a
    // relay is reachable — (re)dispatches each one through the normal send path.
    // The disk entry is the source of truth for the bytes; the overlay is just
    // what's drawn. Removal happens in the ack branch of each dispatch path.
    let flush_offline_queue: Rc<dyn Fn()> = {
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        let pending_state = pending_state.clone();
        let vault_cell = vault_cell.clone();
        let dispatch_send = dispatch_send.clone();
        Rc::new(move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(vault) = vault_cell.lock().ok().and_then(|g| g.clone()) else {
                return;
            };
            let Some(backend) = backend_cell.lock().unwrap().clone() else {
                return;
            };
            let my_id = backend.account().account_id_hex.clone();
            let my_label = my_avatar_label(&backend, &my_id);
            let online = offline_last_connected().load(AtomicOrdering::Relaxed) > 0;
            let chats_messages = ui.get_chats_messages();

            for entry in offline_queue::load_all(&vault) {
                // Only the active account's queue; other accounts' entries wait
                // until that account is the displayed one.
                if !entry.account_id_hex.eq_ignore_ascii_case(&my_id) {
                    continue;
                }
                // Already going out (or being retried) — don't double-dispatch.
                if offline_inflight_contains(&entry.temp_id) {
                    continue;
                }
                let group_hex = entry.group_hex.clone();
                let temp_id = entry.temp_id.clone();
                let existing = pending_state
                    .lock()
                    .unwrap()
                    .find_send(&group_hex, &temp_id);
                let in_overlay = existing.is_some();
                // A red (online hard-failure) bubble is manual-retry-only within a
                // session — don't auto-flush it. (After a restart it isn't in the
                // overlay yet, so it's retried fresh once, then re-reddens if it
                // genuinely still fails.)
                if existing.map(|s| s.failed).unwrap_or(false) {
                    continue;
                }

                // Boot-dedup: a recovered text whose publish actually landed
                // before the previous exit. Only meaningful for entries not yet
                // shown this session (in-session offline failures are guaranteed
                // rolled back by marmot, so never duplicates).
                if !in_overlay
                    && let offline_queue::QueuedKind::Text { text, effect, .. } = &entry.kind
                {
                    let bodies = vec![text.clone(), append_effect_marker(text, *effect)];
                    if looks_already_sent(&backend, &group_hex, &my_id, &bodies, entry.enqueued_at)
                    {
                        offline_queue::remove(&temp_id);
                        continue;
                    }
                }

                // Reconstruct the overlay mirror so we can render the bubble.
                let pending = match &entry.kind {
                    offline_queue::QueuedKind::Text {
                        text,
                        reply_to,
                        effect,
                    } => PendingSend {
                        temp_id: temp_id.clone(),
                        text: text.clone(),
                        failed: false,
                        reply_to: reply_to.clone(),
                        media: Vec::new(),
                        effect: *effect,
                    },
                    offline_queue::QueuedKind::Attachment(m) => PendingSend {
                        temp_id: temp_id.clone(),
                        text: String::new(),
                        failed: false,
                        reply_to: None,
                        media: vec![PendingMedia {
                            file_name: m.file_name.clone(),
                            media_type: m.media_type.clone(),
                            size_bytes: m.bytes.len() as u64,
                            is_image: m.is_image,
                            is_video: mime_is_video(&m.media_type),
                            is_audio: mime_is_audio(&m.media_type),
                            local_preview: None,
                        }],
                        effect: 0,
                    },
                    offline_queue::QueuedKind::Album(ms) => PendingSend {
                        temp_id: temp_id.clone(),
                        text: String::new(),
                        failed: false,
                        reply_to: None,
                        media: ms
                            .iter()
                            .map(|m| PendingMedia {
                                file_name: m.file_name.clone(),
                                media_type: m.media_type.clone(),
                                size_bytes: m.bytes.len() as u64,
                                is_image: m.is_image,
                                is_video: false,
                                is_audio: false,
                                local_preview: None,
                            })
                            .collect(),
                        effect: 0,
                    },
                };

                // Render the pending bubble if it isn't already on screen.
                if !in_overlay {
                    pending_state
                        .lock()
                        .unwrap()
                        .add_send(&group_hex, pending.clone());
                    if let Some(idx) = group_ids
                        .lock()
                        .unwrap()
                        .iter()
                        .position(|g| g == &group_hex)
                    {
                        let row = pending_chat_message(&pending, &my_id, &my_label);
                        with_inner_messages(&chats_messages, idx, |vm| {
                            if find_message_row(vm, &temp_id).is_none() {
                                push_message_grouped(vm, row);
                            }
                        });
                    }
                }

                // Offline: leave it rendered + queued; the watcher re-runs this on
                // reconnect.
                if !online {
                    continue;
                }

                // Guard against a second timer tick re-dispatching this entry: the
                // media spawns only set the in-flight flag inside their deferred
                // event-loop closure, so set it synchronously here too.
                offline_inflight_insert(&temp_id);

                // Online: (re)dispatch from the durable bytes. The overlay bubble
                // already exists, so the media replays skip their own render.
                match entry.kind {
                    offline_queue::QueuedKind::Text {
                        text,
                        reply_to,
                        effect,
                    } => {
                        let parent_id = reply_to.as_ref().map(|(id, _, _)| id.clone());
                        dispatch_send(
                            group_hex,
                            append_effect_marker(&text, effect),
                            temp_id,
                            parent_id,
                        );
                    }
                    offline_queue::QueuedKind::Attachment(m) => {
                        spawn_attachment_send(
                            weak.clone(),
                            backend_cell.clone(),
                            group_ids.clone(),
                            pending_state.clone(),
                            vault_cell.clone(),
                            group_hex,
                            m.file_name,
                            m.media_type,
                            m.bytes,
                            m.is_image,
                            None,
                            Some(temp_id),
                        );
                    }
                    offline_queue::QueuedKind::Album(ms) => {
                        let files: Vec<StagedFile> = ms
                            .into_iter()
                            .map(|m| StagedFile {
                                file_name: m.file_name,
                                media_type: m.media_type,
                                bytes: m.bytes,
                                is_image: m.is_image,
                                preview: None,
                                thumb: None,
                            })
                            .collect();
                        spawn_album_send(
                            weak.clone(),
                            backend_cell.clone(),
                            group_ids.clone(),
                            pending_state.clone(),
                            vault_cell.clone(),
                            group_hex,
                            files,
                            Some(temp_id),
                        );
                    }
                }
            }
        })
    };

    // Background connectivity watcher: polls relay health (a blocking call, so it
    // can't run on the UI thread) and asks the UI to flush on the first
    // backend-ready tick and on every offline→online transition.
    {
        let backend_cell = backend_cell.clone();
        std::thread::spawn(move || {
            let mut prev_connected = 0usize;
            let mut announced_ready = false;
            loop {
                if let Some(backend) = backend_cell.lock().unwrap().clone() {
                    let connected = backend.relay_health().0;
                    offline_last_connected().store(connected, AtomicOrdering::Relaxed);
                    if !announced_ready {
                        // First time the backend is up: render (and, if online,
                        // flush) whatever was queued before this launch.
                        announced_ready = true;
                        offline_flush_requested().store(true, AtomicOrdering::Relaxed);
                    } else if prev_connected == 0 && connected > 0 {
                        offline_flush_requested().store(true, AtomicOrdering::Relaxed);
                    }
                    prev_connected = connected;
                }
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
        });
    }

    // UI-thread consumer: drains the flush request flag the watcher sets. Held in
    // a binding so the timer lives until `run()` returns.
    let _offline_flush_timer = slint::Timer::default();
    {
        let flush = flush_offline_queue.clone();
        _offline_flush_timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_secs(3),
            move || {
                if offline_flush_requested().swap(false, AtomicOrdering::Relaxed) {
                    flush();
                }
            },
        );
    }

    ui.run()?;
    Ok(())
}

// ─── Profile bridge ────────────────────────────────────────────────────

/// Read the profile from the directory cache on the backend runtime (a
/// sqlite read), then apply it on the UI thread.
fn populate_profile_async(ui: &DarkMatterLinux, backend: &Arc<Backend>) {
    let weak = ui.as_weak();
    let b = backend.clone();
    backend.tokio_handle().spawn(async move {
        let profile = b.load_profile();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            populate_profile_from(&b, &ui, profile);
        });
    });
}

fn populate_profile_from(
    backend: &Backend,
    ui: &DarkMatterLinux,
    profile: anyhow::Result<Option<UserProfileMetadata>>,
) {
    let picture_url = match profile {
        Ok(profile) => {
            let url = profile
                .as_ref()
                .and_then(|p| p.picture.clone())
                .unwrap_or_default();
            apply_profile(ui, profile.as_ref());
            url
        }
        Err(e) => {
            eprintln!("[backend] load_profile failed: {e:#}");
            apply_profile(ui, None);
            String::new()
        }
    };
    set_my_avatar(ui, backend);
    // If the URL is empty (or fetch fails), the Avatar falls back to the
    // initials/gradient — no further work needed here. Only clear when a
    // picture is currently bound: redundant writes to `my-av-picture`
    // re-render every outgoing bubble.
    if picture_url.trim().is_empty() {
        if ui.get_my_av_has_picture() {
            ui.set_my_av_has_picture(false);
            ui.set_my_av_picture(slint::Image::default());
        }
    } else {
        fetch_profile_picture(ui, backend, &picture_url);
    }
}

/// Background fetch + decode of the current account's profile picture.
/// `slint::Image` itself is `!Send`, so the worker thread ships raw RGBA
/// pixels + dimensions across the event loop and the actual `Image` is
/// constructed on the UI thread. Cache mirrors that shape.
fn fetch_profile_picture(ui: &DarkMatterLinux, backend: &Backend, url: &str) {
    let url = url.trim().to_string();
    if picture_cache_has(&url) {
        apply_picture(ui, &url);
        return;
    }
    let weak = ui.as_weak();
    let url_for_task = url.clone();
    backend.tokio_handle().spawn(async move {
        let bytes = match reqwest::get(&url_for_task).await {
            Ok(resp) => match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[avatar] download failed for {url_for_task}: {e}");
                    return;
                }
            },
            Err(e) => {
                eprintln!("[avatar] request failed for {url_for_task}: {e}");
                return;
            }
        };
        let pixels = match decode_avatar_pixels(&bytes) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[avatar] decode failed for {url_for_task}: {e}");
                return;
            }
        };
        picture_cache_put(url_for_task.clone(), pixels);
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                apply_picture(&ui, &url_for_task);
            }
        });
    });
}

// ─── Peer profile modal ─────────────────────────────────────────────────

/// Resolve a nostr profile reference ("npub1…", "nprofile1…", or 64-char hex)
/// to an account-id hex. Non-profile entities (nevent/naddr/note) return None
/// so the caller can fall back to the platform URL handler.
fn nostr_ref_to_hex(reference: &str) -> Option<String> {
    if let Ok(pk) = nostr::PublicKey::parse(reference) {
        return Some(pk.to_hex());
    }
    use nostr::nips::nip19::FromBech32;
    nostr::nips::nip19::Nip19Profile::from_bech32(reference)
        .ok()
        .map(|p| p.public_key.to_hex())
}

/// Open the profile modal for `account_id_hex`. Cached directory data (group
/// members, contacts, self) renders instantly; unknown accounts — e.g. an
/// @mention of someone outside the group — get the loading skeleton plus an
/// async relay fetch through the discovery set.
fn open_profile_modal(
    ui: &DarkMatterLinux,
    backend_cell: &Arc<Mutex<Option<Arc<Backend>>>>,
    account_id_hex: &str,
) {
    let guard = backend_cell.lock().unwrap();
    let Some(backend) = guard.as_ref() else {
        return;
    };
    let id = account_id_hex.to_lowercase();
    let is_self = id.eq_ignore_ascii_case(&backend.account().account_id_hex);
    let npub = npub_for_account_id(&id).unwrap_or_else(|_| id.clone());
    let npub_short = shorten_npub(&npub);

    ui.set_peer_profile_account_id(s(&id));
    ui.set_peer_profile_npub(s(&npub));
    ui.set_peer_profile_npub_short(s(&npub_short));
    ui.set_peer_profile_is_self(is_self);
    ui.set_peer_profile_adding(false);
    ui.set_peer_profile_status(s(""));
    ui.set_peer_profile_not_found(false);
    ui.set_peer_profile_picture(slint::Image::default());
    ui.set_peer_profile_has_picture(false);

    // Paint the loading skeleton immediately; follow-list membership and the
    // cached profile are sqlite reads, so they resolve on the runtime and
    // land a beat later (guarded against the modal moving on).
    ui.set_peer_profile_is_contact(false);
    ui.set_peer_profile_loading(true);
    apply_peer_profile(ui, backend, &id, &npub_short, None);
    ui.set_peer_profile_open(true);

    let weak = ui.as_weak();
    let backend_cell = backend_cell.clone();
    let b = backend.clone();
    let npub_short = npub_short.clone();
    backend.tokio_handle().spawn(async move {
        let is_contact = !is_self
            && b.follow_list()
                .map(|l| l.iter().any(|r| r.account_id_hex.eq_ignore_ascii_case(&id)))
                .unwrap_or(false);
        let cached = b.cached_profile(&id);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            // Stale guard: the modal may have closed or moved on to a
            // different user while the lookup was in flight.
            if !ui
                .get_peer_profile_account_id()
                .as_str()
                .eq_ignore_ascii_case(&id)
            {
                return;
            }
            ui.set_peer_profile_is_contact(is_contact);
            if let Some(profile) = cached {
                ui.set_peer_profile_loading(false);
                apply_peer_profile(&ui, &b, &id, &npub_short, Some(&profile));
                return;
            }
            let weak = ui.as_weak();
            let id_done = id.clone();
            let npub_short_done = npub_short.clone();
            b.fetch_profile_async(&id, move |profile| {
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    // Same stale guard for the relay round-trip.
                    if !ui
                        .get_peer_profile_account_id()
                        .as_str()
                        .eq_ignore_ascii_case(&id_done)
                    {
                        return;
                    }
                    ui.set_peer_profile_loading(false);
                    let guard = backend_cell.lock().unwrap();
                    let Some(backend) = guard.as_ref() else {
                        return;
                    };
                    match profile {
                        Some(p) => {
                            apply_peer_profile(&ui, backend, &id_done, &npub_short_done, Some(&p))
                        }
                        None => ui.set_peer_profile_not_found(true),
                    }
                });
            });
        });
    });
}

/// Push a resolved (or placeholder) profile into the modal's properties and
/// kick off the avatar download when a picture URL is present.
fn apply_peer_profile(
    ui: &DarkMatterLinux,
    backend: &Backend,
    account_id_hex: &str,
    npub_short: &str,
    profile: Option<&UserProfileMetadata>,
) {
    let name = profile
        .and_then(|p| {
            p.display_name
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| p.name.clone().filter(|s| !s.is_empty()))
        })
        .unwrap_or_else(|| npub_short.to_string());
    let (a, b, init) = avatar_for(&name);
    ui.set_peer_profile_name(s(&name));
    ui.set_peer_profile_av_a(a);
    ui.set_peer_profile_av_b(b);
    ui.set_peer_profile_av_initials(s(&init));
    ui.set_peer_profile_nip05(s(profile.and_then(|p| p.nip05.as_deref()).unwrap_or("")));
    ui.set_peer_profile_about(s(profile
        .and_then(|p| p.about.as_deref())
        .unwrap_or("")
        .trim()));
    ui.set_peer_profile_lud16(s(profile.and_then(|p| p.lud16.as_deref()).unwrap_or("")));

    let url = profile
        .and_then(|p| p.picture.clone())
        .filter(|u| !u.trim().is_empty());
    if let Some(url) = url {
        let (img, has) = bind_cached_picture(Some(&url));
        ui.set_peer_profile_picture(img);
        ui.set_peer_profile_has_picture(has);
        if !has {
            fetch_peer_profile_picture(ui, backend, account_id_hex, &url);
        }
    }
}

/// Download + decode the modal avatar, then bind it if the modal still shows
/// the same account. Cache-backed; the `slint::Image` is reconstructed on the
/// UI thread because it is `!Send`.
fn fetch_peer_profile_picture(
    ui: &DarkMatterLinux,
    backend: &Backend,
    account_id_hex: &str,
    url: &str,
) {
    let url = url.trim().to_string();
    let id = account_id_hex.to_string();
    let weak = ui.as_weak();
    backend.tokio_handle().spawn(async move {
        let Some(pixels) = fetch_picture_pixels(&url).await else {
            return;
        };
        picture_cache_put(url.clone(), pixels.clone());
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            if !ui
                .get_peer_profile_account_id()
                .as_str()
                .eq_ignore_ascii_case(&id)
            {
                return;
            }
            ui.set_peer_profile_picture(rgba_to_slint_image(&pixels));
            ui.set_peer_profile_has_picture(true);
        });
    });
}

#[derive(Clone)]
struct PicturePixels {
    w: u32,
    h: u32,
    rgba: Vec<u8>,
}

/// Bind the user's own avatar picture by cache key (URL). Uses the shared
/// thread-local `Image` handle and SKIPS the property writes when the handle
/// is already bound: `my-av-picture` feeds the left-rail avatar AND every
/// outgoing bubble, so a fresh handle (or even a redundant set) re-renders
/// the whole conversation — the visible blink reported after background
/// syncs.
fn apply_picture(ui: &DarkMatterLinux, url: &str) {
    let Some(img) = cached_picture_image(url) else {
        return;
    };
    if ui.get_my_av_has_picture() && ui.get_my_av_picture() == img {
        return;
    }
    ui.set_my_av_picture(img);
    ui.set_my_av_has_picture(true);
}

fn picture_cache() -> &'static Mutex<HashMap<String, PicturePixels>> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Mutex<HashMap<String, PicturePixels>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn picture_cache_get(url: &str) -> Option<PicturePixels> {
    picture_cache().lock().ok()?.get(url).cloned()
}

// UI-thread caches of ready `slint::Image` handles. `slint::Image` is `!Send`
// (it wraps a `VRc`), so these mirror the `Send` pixel caches above as
// thread-locals: the first bind converts pixels → image once, and every later
// row build clones the cheap shared handle instead of re-copying the whole
// RGBA buffer. Sharing one handle across rows also means the renderer sees
// one texture per picture instead of one per bubble. Entries never go stale:
// the underlying pixel caches are write-once per key (URLs are
// content-addressed; attachment pixels are keyed by message id).
thread_local! {
    static PICTURE_IMAGES: RefCell<HashMap<String, slint::Image>> = RefCell::new(HashMap::new());
    static ATTACHMENT_IMAGES: RefCell<HashMap<String, slint::Image>> = RefCell::new(HashMap::new());
}

/// Resolve a picture-cache key (URL or `group-image:` key) to a shared
/// `slint::Image`, converting from cached pixels on first use. UI thread only.
fn cached_picture_image(url: &str) -> Option<slint::Image> {
    PICTURE_IMAGES.with(|cache| {
        if let Some(img) = cache.borrow().get(url) {
            return Some(img.clone());
        }
        let pixels = picture_cache_get(url)?;
        let img = rgba_to_slint_image(&pixels);
        cache.borrow_mut().insert(url.to_string(), img.clone());
        Some(img)
    })
}

/// Same as [`cached_picture_image`] but for decrypted image attachments,
/// keyed by message id. UI thread only.
fn cached_attachment_image(id: &str) -> Option<slint::Image> {
    ATTACHMENT_IMAGES.with(|cache| {
        if let Some(img) = cache.borrow().get(id) {
            return Some(img.clone());
        }
        let pixels = attachment_image_cache_get(id)?;
        let img = image_from_pixels(&pixels);
        cache.borrow_mut().insert(id.to_string(), img.clone());
        Some(img)
    })
}

/// Resolve an optional picture URL against the process-wide picture cache,
/// returning a ready-to-render `(Image, has-picture)` pair. A miss yields the
/// default image; callers spawn an async fetch that repopulates the cache and
/// triggers a rebuild so the picture lands on a later frame.
fn bind_cached_picture(url: Option<&str>) -> (slint::Image, bool) {
    url.map(str::trim)
        .filter(|u| !u.is_empty())
        .and_then(cached_picture_image)
        .map(|img| (img, true))
        .unwrap_or((slint::Image::default(), false))
}

/// Map of sender account-id hex → (display name, optional picture URL).
/// Built once per rebuild so rendering N message rows costs one directory read
/// per *unique* sender instead of one per message (keeps the hot path cheap
/// while still resolving real profiles).
type SenderProfiles = std::collections::HashMap<String, (String, Option<String>)>;

fn build_sender_profiles(
    backend: &Backend,
    records: &[AppMessageRecord],
    my_id: &str,
) -> SenderProfiles {
    let mut map = SenderProfiles::new();
    for r in records {
        if r.sender.eq_ignore_ascii_case(my_id) {
            continue;
        }
        map.entry(r.sender.clone())
            .or_insert_with(|| backend.account_name_and_picture(&r.sender));
    }
    map
}

/// How many recent records (all kinds — chat, reactions, edits) are loaded
/// per chat by default. The messages view instantiates a full bubble
/// component tree per visible row (the Slint `for` is eager, not
/// virtualized), so this window is the main lever on chat-switch latency.
/// "Load earlier messages" grows it per chat via [`msg_window_expand`].
const MESSAGE_WINDOW: usize = 80;

/// Per-chat message-window overrides (group_id_hex → record limit). Only
/// chats expanded via "Load earlier messages" have an entry; everything else
/// uses [`MESSAGE_WINDOW`]. Process-wide like the picture caches so the many
/// callback closures don't all need another captured handle.
fn msg_windows() -> &'static Mutex<HashMap<String, usize>> {
    use std::sync::OnceLock;
    static MAP: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Current record limit for a chat (default [`MESSAGE_WINDOW`]).
fn msg_window_for(group_hex: &str) -> usize {
    msg_windows()
        .lock()
        .ok()
        .and_then(|m| m.get(group_hex).copied())
        .unwrap_or(MESSAGE_WINDOW)
}

/// Grow a chat's window by one [`MESSAGE_WINDOW`] step; returns the new limit.
fn msg_window_expand(group_hex: &str) -> usize {
    let mut map = match msg_windows().lock() {
        Ok(m) => m,
        Err(_) => return MESSAGE_WINDOW,
    };
    let w = map.entry(group_hex.to_string()).or_insert(MESSAGE_WINDOW);
    *w += MESSAGE_WINDOW;
    *w
}

/// Drop a chat's expanded window (back to the default). Called on chat
/// select so re-entering a chat is always the fast path.
fn msg_window_reset(group_hex: &str) {
    if let Ok(mut m) = msg_windows().lock() {
        m.remove(group_hex);
    }
}

fn picture_cache_put(url: String, pixels: PicturePixels) {
    if let Ok(mut c) = picture_cache().lock() {
        c.insert(url, pixels);
    }
}

/// Presence check that doesn't clone the pixel buffer out of the cache —
/// `picture_cache_get(url).is_some()` copies the whole RGBA blob just to
/// throw it away.
fn picture_cache_has(url: &str) -> bool {
    picture_cache()
        .lock()
        .map(|c| c.contains_key(url))
        .unwrap_or(false)
}

/// Cache for decrypted+decoded image attachments. Keyed by the inner-event
/// message id so the same bubble can be rebuilt many times (overlay/reaction
/// changes) without losing the loaded image. Populated lazily on the first
/// tap of an image attachment.
fn attachment_image_cache() -> &'static Mutex<HashMap<String, PicturePixels>> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Mutex<HashMap<String, PicturePixels>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn attachment_image_cache_get(id: &str) -> Option<PicturePixels> {
    attachment_image_cache().lock().ok()?.get(id).cloned()
}

fn attachment_image_cache_put(id: String, pixels: PicturePixels) {
    if let Ok(mut c) = attachment_image_cache().lock() {
        c.insert(id, pixels);
    }
}

/// Tracks attachments currently being decrypted (so the UI shows "decrypting…"
/// and so we don't fire duplicate downloads on rapid clicks). Stores
/// message_id_hex while the round-trip is in flight.
fn attachment_in_flight() -> &'static Mutex<std::collections::HashSet<String>> {
    use std::sync::OnceLock;
    static SET: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Convert cached pixels into a Slint `Image`. Must be called on the UI thread —
/// `slint::Image` is `!Send` (it wraps a `VRc`).
fn image_from_pixels(pixels: &PicturePixels) -> slint::Image {
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        &pixels.rgba,
        pixels.w,
        pixels.h,
    );
    slint::Image::from_rgba8(buffer)
}

/// One image in the lightbox slideshow. `cache_key` is the shared attachment
/// cache handle (bare message id for a lone image, `id#index` for an album
/// member) — the same key the bubble renders from. `reference` is pre-resolved
/// so prev/next never re-reads sqlite to find what to download.
#[derive(Clone)]
struct ViewerItem {
    cache_key: String,
    reference: MediaAttachmentReference,
}

/// Every image in the chat window as an ordered slideshow list — one item per
/// image, expanding albums into their members. `cache_key` matches the render
/// path: a lone image keeps the bare message id; album members get `id#index`.
fn build_viewer_items(all: &[AppMessageRecord]) -> Vec<ViewerItem> {
    let mut items = Vec::new();
    for m in all {
        let imgs: Vec<MediaAttachmentReference> =
            parse_all_media_references(&m.tags, m.source_epoch)
                .into_iter()
                .filter(|r| mime_is_image(&r.media_type))
                .collect();
        if imgs.len() == 1 {
            items.push(ViewerItem {
                cache_key: m.message_id_hex.clone(),
                reference: imgs.into_iter().next().unwrap(),
            });
        } else {
            for (i, reference) in imgs.into_iter().enumerate() {
                items.push(ViewerItem {
                    cache_key: att_key(&m.message_id_hex, i),
                    reference,
                });
            }
        }
    }
    items
}

/// Ordered image attachments for the open lightbox + the current position.
/// UI-thread-only state (the lightbox and its callbacks all run there), held
/// in a thread-local so download-completion closures — which must be `Send`
/// to cross `invoke_from_event_loop` and so can't capture an `Rc` — can still
/// reach it once they hop back onto the event loop.
#[derive(Default)]
struct ViewerSlideshow {
    items: Vec<ViewerItem>,
    pos: usize,
}

thread_local! {
    static VIEWER_SLIDESHOW: std::cell::RefCell<ViewerSlideshow> =
        std::cell::RefCell::new(ViewerSlideshow::default());
}

/// Build the slideshow list for the open lightbox: every image attachment in
/// the chat window, in message order, with the tapped one selected. The
/// sqlite read + tag parse run on the backend runtime; the result (which is
/// `Send`) hops back to store the list and seed the counter.
fn build_viewer_slideshow(
    weak: slint::Weak<DarkMatterLinux>,
    backend_cell: Arc<Mutex<Option<Arc<Backend>>>>,
    group_ids: Arc<Mutex<Vec<String>>>,
    group_hex: String,
    current_key: String,
) {
    let Some(backend) = backend_cell.lock().unwrap().clone() else {
        return;
    };
    let handle = backend.tokio_handle();
    handle.spawn(async move {
        let all = backend
            .messages(&group_hex, Some(msg_window_for(&group_hex)))
            .unwrap_or_default();
        let items = build_viewer_items(&all);
        let pos = items
            .iter()
            .position(|it| it.cache_key == current_key)
            .unwrap_or(0);
        let count = items.len();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            // Store the list, then load the selected image (cache hit → instant,
            // miss → loading pill + download).
            let current = VIEWER_SLIDESHOW.with(|s| {
                *s.borrow_mut() = ViewerSlideshow { items, pos };
                s.borrow().items.get(pos).cloned()
            });
            ui.set_image_viewer_count(count as i32);
            ui.set_image_viewer_index((pos + 1) as i32);
            if let Some(item) = current {
                load_viewer_image(&ui, &backend_cell, &group_ids, pos, item);
            } else {
                ui.set_image_viewer_loading(false);
            }
        });
    });
}

/// Show the image at slideshow position `pos` in the lightbox. Cache hit →
/// swap instantly; miss → flip on the loading pill and download+decode, then
/// swap *only if* the viewer is still parked on the same image (the user may
/// have clicked past it). The decoded pixels seed the shared attachment cache
/// so the bubble row and a re-open are both free afterwards.
fn load_viewer_image(
    ui: &DarkMatterLinux,
    backend_cell: &Arc<Mutex<Option<Arc<Backend>>>>,
    group_ids: &Arc<Mutex<Vec<String>>>,
    pos: usize,
    item: ViewerItem,
) {
    if let Some(pixels) = attachment_image_cache_get(&item.cache_key) {
        ui.set_image_viewer_image(image_from_pixels(&pixels));
        ui.set_image_viewer_loading(false);
        return;
    }
    ui.set_image_viewer_loading(true);
    let idx = ui.get_active_chat() as usize;
    let Some(group_hex) = group_ids.lock().unwrap().get(idx).cloned() else {
        return;
    };
    let Some(backend) = backend_cell.lock().unwrap().clone() else {
        return;
    };
    let weak = ui.as_weak();
    let mid = item.cache_key.clone();
    backend.download_media_async(&group_hex, item.reference, move |result| {
        // Runs on the backend runtime. Decode here, hop to the UI thread to
        // build the (!Send) Image and apply it.
        let pixels = match result {
            Ok(dl) => image::load_from_memory(&dl.plaintext).ok().map(|img| {
                let rgba = img.to_rgba8();
                PicturePixels {
                    w: rgba.width(),
                    h: rgba.height(),
                    rgba: rgba.into_raw(),
                }
            }),
            Err(e) => {
                eprintln!("[viewer] download {mid}: {e:#}");
                None
            }
        };
        if let Some(px) = &pixels {
            attachment_image_cache_put(mid.clone(), px.clone());
        }
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            // Drop the result if the user navigated on while we downloaded.
            let still_current = VIEWER_SLIDESHOW.with(|s| {
                let s = s.borrow();
                s.pos == pos && s.items.get(pos).is_some_and(|it| it.cache_key == mid)
            });
            if !still_current {
                return;
            }
            if let Some(px) = pixels {
                ui.set_image_viewer_image(image_from_pixels(&px));
            }
            ui.set_image_viewer_loading(false);
        });
    });
}

/// Resolve a chat record's NIP-92 `imeta` tag into a [`MediaAttachmentReference`]
/// (encrypted-media v1). The tag carries repeatable `locator <kind> <value>`
/// entries plus `ciphertext_sha256` / `plaintext_sha256` / `nonce` / `m` /
/// `filename` / `v` and optional `dim` / `thumbhash`. `source_epoch` is the
/// message's MLS epoch (from the record), needed to derive the per-epoch media
/// secret on download. Returns None when the tag is absent or missing a
/// required field.
fn parse_media_reference_from_tags(
    tags: &[Vec<String>],
    source_epoch: Option<u64>,
) -> Option<MediaAttachmentReference> {
    tags.iter()
        .find(|t| t.first().map(String::as_str) == Some("imeta"))
        .and_then(|t| parse_one_imeta(t, source_epoch))
}

/// Every `imeta` reference on a record, in tag order. A multi-image message
/// (album) carries one `imeta` tag per image; the single-attachment path only
/// ever looks at the first via [`parse_media_reference_from_tags`].
fn parse_all_media_references(
    tags: &[Vec<String>],
    source_epoch: Option<u64>,
) -> Vec<MediaAttachmentReference> {
    tags.iter()
        .filter(|t| t.first().map(String::as_str) == Some("imeta"))
        .filter_map(|t| parse_one_imeta(t, source_epoch))
        .collect()
}

/// Parse a single `imeta` tag (already known to start with "imeta") into a
/// [`MediaAttachmentReference`]. Returns None when a required field is absent.
fn parse_one_imeta(tag: &[String], source_epoch: Option<u64>) -> Option<MediaAttachmentReference> {
    if tag.first().map(String::as_str) != Some("imeta") {
        return None;
    }
    let mut locators = Vec::new();
    let mut fields: HashMap<String, String> = HashMap::new();
    for field in tag.iter().skip(1) {
        if let Some(rest) = field.strip_prefix("locator ") {
            if let Some((kind, value)) = rest.split_once(' ') {
                locators.push(MediaLocator {
                    kind: kind.to_string(),
                    value: value.to_string(),
                });
            }
            continue;
        }
        if let Some((k, v)) = field.split_once(' ') {
            fields.insert(k.to_string(), v.to_string());
        }
    }
    if locators.is_empty() {
        return None;
    }
    Some(MediaAttachmentReference {
        locators,
        ciphertext_sha256: fields.get("ciphertext_sha256")?.clone(),
        plaintext_sha256: fields.get("plaintext_sha256")?.clone(),
        nonce_hex: fields.get("nonce")?.clone(),
        file_name: fields.get("filename")?.clone(),
        media_type: fields.get("m")?.clone(),
        version: fields.get("v").cloned().unwrap_or_default(),
        source_epoch: source_epoch.unwrap_or_default(),
        dim: fields.get("dim").cloned(),
        thumbhash: fields.get("thumbhash").cloned(),
    })
}

fn mime_is_image(mime: &str) -> bool {
    mime.starts_with("image/")
}

fn mime_is_video(mime: &str) -> bool {
    mime.starts_with("video/")
}

fn mime_is_audio(mime: &str) -> bool {
    mime.starts_with("audio/")
}

/// Attachment-image-cache key for a video's poster frame. Distinct from the
/// bare message id (which the image path uses) so a video never trips the
/// image lightbox's "already decoded → open viewer" shortcut in
/// `on_attachment_clicked`.
fn vidposter_key(message_id: &str) -> String {
    format!("vidposter:{message_id}")
}

/// Format a duration in seconds as "m:ss" (or "h:mm:ss").
fn fmt_dur(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "0:00".to_string();
    }
    let total = secs.round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Cached duration label per video message id ("1:23"), captured the first
/// time a clip is played (mpv reports `duration`). Renders on the poster tile.
fn video_meta() -> &'static Mutex<HashMap<String, String>> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

fn video_duration_label(message_id: &str) -> String {
    video_meta()
        .lock()
        .ok()
        .and_then(|m| m.get(message_id).cloned())
        .unwrap_or_default()
}

/// The single live [`mpv::MpvPlayer`] backing the video viewer. Only one video
/// plays at a time; opening another or dismissing the viewer drops this (which
/// joins the render/event threads and frees the mpv handle).
fn current_player() -> &'static Mutex<Option<mpv::MpvPlayer>> {
    use std::sync::OnceLock;
    static P: OnceLock<Mutex<Option<mpv::MpvPlayer>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(None))
}

/// Stop + drop the live player off the UI thread. `MpvPlayer::drop` joins its
/// render/event threads and calls `mpv_terminate_destroy`, which can block
/// briefly — never do that on the event loop (hard rule).
fn stop_current_player() {
    let taken = current_player().lock().ok().and_then(|mut p| p.take());
    if let Some(player) = taken {
        std::thread::spawn(move || drop(player));
    }
}

/// Duration (seconds) of the currently-open video, for translating the seek
/// bar's 0..1 fraction into an absolute position.
fn current_video_duration() -> &'static Mutex<f64> {
    use std::sync::OnceLock;
    static D: OnceLock<Mutex<f64>> = OnceLock::new();
    D.get_or_init(|| Mutex::new(0.0))
}

/// `(group_hex, message_id)` of the video currently open in the viewer, so the
/// dismiss handler can repaint that bubble (poster + duration now cached).
fn current_video_target() -> &'static Mutex<Option<(String, String)>> {
    use std::sync::OnceLock;
    static T: OnceLock<Mutex<Option<(String, String)>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(None))
}

/// Whether the video viewer put the app window into fullscreen. Tracked so the
/// `f`-key / button toggle can flip it and the dismiss handler can revert it
/// (so closing the viewer never leaves the whole app stuck fullscreen).
fn video_fullscreen() -> &'static std::sync::atomic::AtomicBool {
    use std::sync::OnceLock;
    static F: OnceLock<std::sync::atomic::AtomicBool> = OnceLock::new();
    F.get_or_init(|| std::sync::atomic::AtomicBool::new(false))
}

/// Fetch (cache read-through, else decrypt+download) a video attachment and
/// hand the bytes to [`spawn_video_player`]. Runs entirely on the backend
/// runtime; on failure it just clears the viewer's loading spinner.
fn start_video_playback(
    weak: Weak<DarkMatterLinux>,
    backend: Arc<Backend>,
    group_hex: String,
    mid: String,
    reference: MediaAttachmentReference,
    vault: Option<Arc<Mutex<Vault>>>,
) {
    let hash = reference.ciphertext_sha256.clone();
    let backend2 = backend.clone();
    backend.tokio_handle().spawn(async move {
        // Encrypted disk cache first (survives restart, no network).
        if let Some(bytes) = vault.as_ref().and_then(|v| media_cache::get(v, &hash)) {
            spawn_video_player(weak, mid, bytes);
            return;
        }
        let weak_fail = weak.clone();
        let mid_dl = mid.clone();
        let vault2 = vault.clone();
        backend2.download_media_async(&group_hex, reference, move |res| match res {
            Ok(dl) => {
                if let Some(v) = &vault2 {
                    media_cache::put(v, &hash, &dl.plaintext);
                }
                spawn_video_player(weak, mid_dl, dl.plaintext);
            }
            Err(e) => {
                eprintln!("[video] download {mid_dl}: {e:#}");
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak_fail.upgrade() {
                        ui.set_video_viewer_loading(false);
                    }
                });
            }
        });
    });
}

/// Build the libmpv player for already-decrypted `bytes`, wiring its frame and
/// state callbacks to the video viewer. Caches the first frame as the bubble
/// poster and the clip duration. Stores the player in [`current_player`] so the
/// viewer controls + dismiss can reach it. Safe to call off the UI thread.
fn spawn_video_player(weak: Weak<DarkMatterLinux>, mid: String, bytes: Vec<u8>) {
    use std::sync::atomic::{AtomicBool, Ordering};

    let poster_saved = Arc::new(AtomicBool::new(false));
    let dur_saved = Arc::new(AtomicBool::new(false));

    let on_frame = {
        let weak = weak.clone();
        let mid = mid.clone();
        move |px: PicturePixels| {
            // First frame doubles as the bubble poster (cached once).
            if !poster_saved.swap(true, Ordering::AcqRel) {
                attachment_image_cache_put(vidposter_key(&mid), px.clone());
            }
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_video_viewer_frame(image_from_pixels(&px));
                    ui.set_video_viewer_has_frame(true);
                    ui.set_video_viewer_loading(false);
                }
            });
        }
    };

    let on_state = {
        let weak = weak.clone();
        let mid = mid.clone();
        move |st: mpv::PlayerState| {
            if st.duration > 0.0 {
                *current_video_duration().lock().unwrap() = st.duration;
                if !dur_saved.swap(true, Ordering::AcqRel)
                    && let Ok(mut m) = video_meta().lock()
                {
                    m.insert(mid.clone(), fmt_dur(st.duration));
                }
            }
            let progress = if st.duration > 0.0 {
                (st.time_pos / st.duration).clamp(0.0, 1.0) as f32
            } else {
                0.0
            };
            let pos_l = fmt_dur(st.time_pos);
            let dur_l = fmt_dur(st.duration);
            let playing = !st.paused;
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_video_viewer_progress(progress);
                    ui.set_video_viewer_pos(pos_l.into());
                    ui.set_video_viewer_dur(dur_l.into());
                    ui.set_video_viewer_playing(playing);
                }
            });
        }
    };

    match mpv::MpvPlayer::open(bytes, 1920, on_frame, on_state) {
        Some(player) => {
            *current_player().lock().unwrap() = Some(player);
        }
        None => {
            eprintln!("[video] mpv player failed to start");
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_video_viewer_loading(false);
                }
            });
        }
    }
}

// ─── Voice-message playback helpers ─────────────────────────────────────────

/// Stop any active voice-message playback. Must be called from the Slint UI
/// thread because the player is !Send.
fn stop_current_audio() {
    with_active_player(|p| {
        *p = None;
    });
    *current_audio_message_id().lock().unwrap() = None;
}

/// Start playing an audio attachment. `bytes` are the decrypted WAV data.
/// A monitor thread keeps the playing message's bubble refreshed with
/// position/duration. When playback finishes or another message is started,
/// the bubble is updated accordingly.
fn start_audio_playback(
    weak: Weak<DarkMatterLinux>,
    backend_cell: Arc<Mutex<Option<Arc<Backend>>>>,
    group_ids: Arc<Mutex<Vec<String>>>,
    pending_state: Arc<Mutex<PendingState>>,
    group_hex: String,
    message_id: String,
    bytes: Vec<u8>,
) {
    stop_current_audio();
    let player = match audio::AudioPlayer::play(bytes) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[audio] play {message_id}: {e:#}");
            return;
        }
    };

    let mid = message_id.clone();
    let weak2 = weak.clone();
    let backend_cell_c = backend_cell.clone();
    let group_ids_c = group_ids.clone();
    let pending_state_c = pending_state.clone();
    let group_hex_c = group_hex.clone();
    player.spawn_monitor(move |st: audio::PlaybackState| {
        let progress = if st.duration > 0.0 {
            (st.position / st.duration).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };
        {
            let mut cache = audio_progress().lock().unwrap();
            if st.finished {
                cache.insert(mid.clone(), 0.0);
            } else {
                cache.insert(mid.clone(), progress);
            }
        }
        if st.duration > 0.0
            && let Ok(mut m) = audio_meta().lock()
        {
            m.insert(mid.clone(), fmt_dur(st.duration));
        }
        let finished = st.finished;
        let mid_i = mid.clone();
        let mid_fin = mid.clone();
        let weak_i = weak2.clone();
        let backend_cell_i = backend_cell_c.clone();
        let group_ids_i = group_ids_c.clone();
        let pending_state_i = pending_state_c.clone();
        let group_hex_i = group_hex_c.clone();
        let _ = slint::invoke_from_event_loop(move || {
            // A drained clip is no longer the active player. Drop it (only if it
            // is still the current one — a newer clip may have started) so the
            // next play-button press restarts from the top instead of toggling
            // an empty sink, which does nothing.
            let still_current = finished
                && current_audio_message_id().lock().unwrap().as_deref() == Some(mid_fin.as_str());
            if still_current {
                stop_current_audio();
            }
            if let Some(ui) = weak_i.upgrade() {
                // Refresh the bubble so the play button / progress bar updates.
                let idx = ui.get_active_chat() as usize;
                let group_opt = {
                    let ids = group_ids_i.lock().unwrap();
                    ids.get(idx).cloned()
                };
                if let Some(g) = group_opt
                    && g == group_hex_i
                    && let Some(backend) = backend_cell_i.lock().unwrap().clone()
                {
                    refresh_one_message_row_async(
                        &backend,
                        weak_i,
                        pending_state_i,
                        group_ids_i,
                        group_hex_i,
                        mid_i,
                    );
                }
            }
        });
    });

    with_active_player(|p| {
        *p = Some(player);
    });
    *current_audio_message_id().lock().unwrap() = Some(message_id);
}

// ─── Album (multi-image) layout + cells ────────────────────────────────────

/// Per-image cache/slideshow key. A lone attachment keeps the bare message id
/// (back-compat with the single-image path); album images are `id#index`.
fn att_key(message_id: &str, index: usize) -> String {
    format!("{message_id}#{index}")
}

/// Aspect ratio (w/h) from an `imeta` `dim "WxH"` field, if present + valid.
fn parse_dim_ar(dim: &Option<String>) -> Option<f32> {
    let d = dim.as_ref()?;
    let (w, h) = d.split_once(['x', 'X'])?;
    let w: f32 = w.trim().parse().ok()?;
    let h: f32 = h.trim().parse().ok()?;
    (w > 0.0 && h > 0.0).then_some(w / h)
}

const ALBUM_GAP: f32 = 3.0;
const ALBUM_MAX_H: f32 = 460.0;

/// Telegram-style aspect-aware grid. Given each image's aspect ratio, lay the
/// album into a box `max_w` wide and return per-cell px rects `(x, y, w, h)`
/// plus the total height. Special cases for 2/3 images (the eye-catching
/// arrangements); a balanced justified-rows fallback for 4+. The whole grid is
/// scaled down if it would exceed `ALBUM_MAX_H`.
fn album_layout(aspects: &[f32], max_w: f32, sp: f32) -> (Vec<(f32, f32, f32, f32)>, f32) {
    let n = aspects.len();
    // Clamp extreme panoramas/strips so one wild image can't wreck the grid.
    let ar: Vec<f32> = aspects.iter().map(|a| a.clamp(0.5, 2.4)).collect();
    let (mut out, total_h): (Vec<(f32, f32, f32, f32)>, f32) = match n {
        0 => (vec![], 0.0),
        1 => {
            let h = (max_w / ar[0]).clamp(max_w * 0.5, max_w * 1.4);
            (vec![(0.0, 0.0, max_w, h)], h)
        }
        2 if ar[0] > 1.2 && ar[1] > 1.2 => {
            // Two wide images read best stacked full-width.
            let h0 = max_w / ar[0];
            let h1 = max_w / ar[1];
            (
                vec![(0.0, 0.0, max_w, h0), (0.0, h0 + sp, max_w, h1)],
                h0 + sp + h1,
            )
        }
        2 => {
            // Two equal-height columns filling the width.
            let h = (max_w - sp) / (ar[0] + ar[1]);
            let w0 = h * ar[0];
            (
                vec![(0.0, 0.0, w0, h), (w0 + sp, 0.0, (max_w - sp) - w0, h)],
                h,
            )
        }
        3 if ar[0] >= 1.0 => {
            // One wide image on top, two columns below.
            let h0 = (max_w / ar[0]).clamp(max_w * 0.4, max_w * 0.9);
            let hb = (max_w - sp) / (ar[1] + ar[2]);
            let w1 = hb * ar[1];
            (
                vec![
                    (0.0, 0.0, max_w, h0),
                    (0.0, h0 + sp, w1, hb),
                    (w1 + sp, h0 + sp, (max_w - sp) - w1, hb),
                ],
                h0 + sp + hb,
            )
        }
        3 => {
            // One tall image on the left, two stacked on the right.
            let inv = 1.0 / ar[1] + 1.0 / ar[2];
            let wr = ((max_w - sp - ar[0] * sp) / (ar[0] * inv + 1.0)).clamp(50.0, max_w - 70.0);
            let wl = max_w - sp - wr;
            let big_h = wl / ar[0];
            let h1 = (wr / ar[1]).min(big_h - sp - 20.0).max(20.0);
            let h2 = (big_h - sp - h1).max(20.0);
            (
                vec![
                    (0.0, 0.0, wl, big_h),
                    (wl + sp, 0.0, wr, h1),
                    (wl + sp, h1 + sp, wr, h2),
                ],
                big_h,
            )
        }
        _ => {
            // Balanced justified rows: split into ~sqrt(n) rows, earlier rows
            // take the remainder, each row justified to fill `max_w`.
            let rows = ((n as f32).sqrt().round() as usize).clamp(2, n);
            let base = n / rows;
            let extra = n % rows;
            let mut sizes = vec![base; rows];
            for s in sizes.iter_mut().take(extra) {
                *s += 1;
            }
            let mut rects = Vec::with_capacity(n);
            let (mut y, mut idx) = (0.0_f32, 0usize);
            for &k in &sizes {
                let row_ar: f32 = (0..k).map(|j| ar[idx + j]).sum();
                let avail = max_w - sp * ((k as f32) - 1.0);
                let h = avail / row_ar;
                let mut x = 0.0_f32;
                for j in 0..k {
                    // Last cell absorbs rounding so the row fills exactly.
                    let w = if j == k - 1 {
                        max_w - x
                    } else {
                        h * ar[idx + j]
                    };
                    rects.push((x, y, w, h));
                    x += w + sp;
                }
                y += h + sp;
                idx += k;
            }
            (rects, (y - sp).max(0.0))
        }
    };
    if total_h > ALBUM_MAX_H {
        let scale = ALBUM_MAX_H / total_h;
        for r in out.iter_mut() {
            r.0 *= scale;
            r.1 *= scale;
            r.2 *= scale;
            r.3 *= scale;
        }
        return (out, ALBUM_MAX_H);
    }
    (out, total_h)
}

/// Album width for a bubble (outgoing bubbles are narrower than incoming).
fn album_box_w(outgoing: bool) -> f32 {
    if outgoing { 360.0 } else { 380.0 }
}

/// Build the grid cells for a confirmed album message: geometry from each
/// image's `dim` (or cached pixels, or square), images from the attachment
/// cache (placeholder until a cell decodes).
fn build_album_cells(
    references: &[MediaAttachmentReference],
    message_id: &str,
    outgoing: bool,
) -> (Vec<AlbumCell>, f32, f32) {
    let max_w = album_box_w(outgoing);
    let aspects: Vec<f32> = references
        .iter()
        .enumerate()
        .map(|(i, r)| {
            parse_dim_ar(&r.dim)
                .or_else(|| {
                    attachment_image_cache_get(&att_key(message_id, i))
                        .map(|p| p.w as f32 / (p.h.max(1) as f32))
                })
                .unwrap_or(1.0)
        })
        .collect();
    let (rects, total_h) = album_layout(&aspects, max_w, ALBUM_GAP);
    let cells = references
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let key = att_key(message_id, i);
            let (image, has_image) = match attachment_image_cache_get(&key) {
                Some(p) => (image_from_pixels(&p), true),
                None => (slint::Image::default(), false),
            };
            let loading = attachment_in_flight()
                .lock()
                .map(|s| s.contains(&key))
                .unwrap_or(false);
            let (x, y, w, h) = rects[i];
            AlbumCell {
                x,
                y,
                w,
                h,
                image,
                has_image,
                loading,
                key: key.into(),
            }
        })
        .collect();
    (cells, max_w, total_h)
}

/// Build the grid cells for a pending (optimistic) album from the local
/// previews the user picked — always rendered, no download needed.
fn pending_album_cells(
    media: &[PendingMedia],
    temp_id: &str,
    outgoing: bool,
) -> (Vec<AlbumCell>, f32, f32) {
    let max_w = album_box_w(outgoing);
    let aspects: Vec<f32> = media
        .iter()
        .map(|m| {
            m.local_preview
                .as_ref()
                .map(|p| p.w as f32 / (p.h.max(1) as f32))
                .unwrap_or(1.0)
        })
        .collect();
    let (rects, total_h) = album_layout(&aspects, max_w, ALBUM_GAP);
    let cells = media
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let (image, has_image) = match &m.local_preview {
                Some(p) => (image_from_pixels(p), true),
                None => (slint::Image::default(), false),
            };
            let (x, y, w, h) = rects[i];
            AlbumCell {
                x,
                y,
                w,
                h,
                image,
                has_image,
                loading: false,
                key: att_key(temp_id, i).into(),
            }
        })
        .collect();
    (cells, max_w, total_h)
}

/// Empty album fields for the common non-album row.
fn no_album() -> (ModelRc<AlbumCell>, f32, f32) {
    (
        ModelRc::new(VecModel::from(Vec::<AlbumCell>::new())),
        0.0,
        0.0,
    )
}

/// Handles album auto-load needs, stashed once at startup. UI-thread-only
/// (a thread-local), so the otherwise-pure row builders can trigger
/// background image fetches without threading these through every signature.
#[derive(Clone)]
struct AlbumLoadCtx {
    weak: Weak<DarkMatterLinux>,
    backend_cell: Arc<Mutex<Option<Arc<Backend>>>>,
    vault_cell: Arc<Mutex<Option<Arc<Mutex<Vault>>>>>,
    group_ids: Arc<Mutex<Vec<String>>>,
    pending_state: Arc<Mutex<PendingState>>,
}

thread_local! {
    static ALBUM_LOAD_CTX: std::cell::RefCell<Option<AlbumLoadCtx>> =
        const { std::cell::RefCell::new(None) };
}

fn set_album_load_ctx(ctx: AlbumLoadCtx) {
    ALBUM_LOAD_CTX.with(|c| *c.borrow_mut() = Some(ctx));
}

/// For an album record (2+ images), kick off background download+decode for
/// any cell that isn't already cached — so incoming albums, and our own
/// albums after a restart cleared the in-memory cache, fill their grid in
/// instead of showing placeholders. No-op for cached/in-flight cells. Each
/// finished cell seeds the in-memory + disk caches and refreshes its row.
/// Reads through the encrypted disk cache before paying for a download.
fn maybe_autoload_album(group_hex: &str, record: &AppMessageRecord) {
    let images: Vec<MediaAttachmentReference> =
        parse_all_media_references(&record.tags, record.source_epoch)
            .into_iter()
            .filter(|r| mime_is_image(&r.media_type))
            .collect();
    if images.len() < 2 {
        return;
    }
    // Which cells still need fetching? Cheap checks only — the attachment
    // cache + in-flight set are independent mutexes. Crucially we do NOT lock
    // `backend_cell` here: this runs from the row builders, which are usually
    // called while the caller already holds that guard, and `std::sync::Mutex`
    // is non-reentrant — re-locking it on the same thread would deadlock.
    let needed: Vec<(usize, MediaAttachmentReference)> = images
        .into_iter()
        .enumerate()
        .filter(|(i, _)| {
            let key = att_key(&record.message_id_hex, *i);
            attachment_image_cache_get(&key).is_none()
                && !attachment_in_flight()
                    .lock()
                    .map(|s| s.contains(&key))
                    .unwrap_or(true)
        })
        .collect();
    if needed.is_empty() {
        return;
    }
    // Defer the backend acquisition + downloads to a fresh event-loop turn so
    // any `backend_cell` guard held by the current row build is released first.
    let group_hex = group_hex.to_string();
    let mid = record.message_id_hex.clone();
    let _ = slint::invoke_from_event_loop(move || autoload_album_cells(group_hex, mid, needed));
}

/// Backend half of [`maybe_autoload_album`], run on its own event-loop turn so
/// no row-builder `backend_cell` guard is in scope. Disk read-through →
/// download → seed caches → refresh the row, per still-missing cell.
fn autoload_album_cells(
    group_hex: String,
    mid: String,
    needed: Vec<(usize, MediaAttachmentReference)>,
) {
    let Some(ctx) = ALBUM_LOAD_CTX.with(|c| c.borrow().clone()) else {
        return;
    };
    let Some(backend) = ctx.backend_cell.lock().unwrap().clone() else {
        return;
    };
    let vault = ctx.vault_cell.lock().unwrap().clone();
    for (i, reference) in needed {
        let key = att_key(&mid, i);
        if attachment_image_cache_get(&key).is_some() {
            continue;
        }
        {
            let Ok(mut set) = attachment_in_flight().lock() else {
                continue;
            };
            if set.contains(&key) {
                continue;
            }
            set.insert(key.clone());
        }
        let backend = backend.clone();
        let vault = vault.clone();
        let group_hex = group_hex.clone();
        let mid = mid.clone();
        let weak = ctx.weak.clone();
        let group_ids = ctx.group_ids.clone();
        let pending_state = ctx.pending_state.clone();
        let hash = reference.ciphertext_sha256.clone();
        backend.tokio_handle().spawn({
            let backend = backend.clone();
            async move {
                // Disk read-through: skip the network if we cached the bytes.
                if let Some(px) = vault
                    .as_ref()
                    .and_then(|v| media_cache::get(v, &hash))
                    .and_then(|plain| decode_avatar_pixels(&plain).ok())
                {
                    attachment_image_cache_put(key.clone(), px);
                    attachment_in_flight()
                        .lock()
                        .ok()
                        .map(|mut s| s.remove(&key));
                    refresh_one_message_row_async(
                        &backend,
                        weak,
                        pending_state,
                        group_ids,
                        group_hex,
                        mid,
                    );
                    return;
                }
                let backend_cb = backend.clone();
                let group_hex_cb = group_hex.clone();
                backend.download_media_async(&group_hex, reference, move |result| {
                    let pixels = match result {
                        Ok(dl) => {
                            if let Some(v) = &vault {
                                media_cache::put(v, &hash, &dl.plaintext);
                            }
                            decode_avatar_pixels(&dl.plaintext).ok()
                        }
                        Err(e) => {
                            eprintln!("[album] autoload {key}: {e:#}");
                            None
                        }
                    };
                    let ok = pixels.is_some();
                    if let Some(px) = pixels {
                        attachment_image_cache_put(key.clone(), px);
                    }
                    attachment_in_flight()
                        .lock()
                        .ok()
                        .map(|mut s| s.remove(&key));
                    if ok {
                        refresh_one_message_row_async(
                            &backend_cb,
                            weak,
                            pending_state,
                            group_ids,
                            group_hex_cb,
                            mid,
                        );
                    }
                });
            }
        });
    }
}

/// Build a [`StagedFile`] from raw bytes: full-resolution decode for the
/// optimistic bubble preview plus a ≤96px thumbnail for the composer chip.
/// Blocking image decode — call off the UI thread.
fn staged_file_from_bytes(file_name: String, media_type: String, bytes: Vec<u8>) -> StagedFile {
    let is_image = mime_is_image(&media_type);
    let (preview, thumb) = if is_image {
        match image::load_from_memory(&bytes) {
            Ok(img) => {
                let t = img.thumbnail(96, 96).to_rgba8();
                let thumb = PicturePixels {
                    w: t.width(),
                    h: t.height(),
                    rgba: t.into_raw(),
                };
                let rgba = img.to_rgba8();
                let (w, h) = (rgba.width(), rgba.height());
                (
                    Some(PicturePixels {
                        w,
                        h,
                        rgba: rgba.into_raw(),
                    }),
                    Some(thumb),
                )
            }
            Err(_) => (None, None),
        }
    } else {
        (None, None)
    };
    StagedFile {
        file_name,
        media_type,
        bytes,
        is_image,
        preview,
        thumb,
    }
}

/// Rebuild the composer's staged-attachment chip row from the queue. UI
/// thread only — it constructs `slint::Image` thumbnails.
fn refresh_staged_ui(ui: &DarkMatterLinux, staged: &[StagedFile]) {
    let rows: Vec<StagedAttachment> = staged
        .iter()
        .map(|f| StagedAttachment {
            name: f.file_name.clone().into(),
            size_label: human_bytes(f.bytes.len() as u64).into(),
            is_image: f.is_image,
            thumb: f.thumb.as_ref().map(image_from_pixels).unwrap_or_default(),
            has_thumb: f.thumb.is_some(),
        })
        .collect();
    ui.set_composer_staged(ModelRc::new(VecModel::from(rows)));
}

/// Compact byte-size label for attachment chips. KB/MB rounded to one decimal.
/// Map on-disk audit-log files into UI rows (newest first) and push the model.
fn push_audit_files(ui: &DarkMatterLinux, mut files: Vec<AuditLogFile>) {
    files.sort_by(|a, b| {
        b.modified_at_ms
            .unwrap_or(0)
            .cmp(&a.modified_at_ms.unwrap_or(0))
    });
    let rows: Vec<AuditLogEntry> = files
        .iter()
        .map(|f| AuditLogEntry {
            path: f.path.clone().into(),
            name: f.file_name.clone().into(),
            meta: match f.modified_at_ms {
                Some(ms) => format!(
                    "{} · {}",
                    human_bytes(f.size_bytes),
                    format_date_unix(ms / 1000)
                )
                .into(),
                None => human_bytes(f.size_bytes).into(),
            },
        })
        .collect();
    ui.set_audit_files(ModelRc::new(VecModel::from(rows)));
}

/// List audit-log files off the UI thread (disk IO) and push the rows back
/// through the event loop.
fn refresh_audit_files(ui: &DarkMatterLinux, backend: &Arc<Backend>) {
    let weak = ui.as_weak();
    let b = backend.clone();
    backend.tokio_handle().spawn(async move {
        let files = b.audit_log_files().unwrap_or_else(|e| {
            eprintln!("[settings] list audit logs failed: {e:#}");
            Vec::new()
        });
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                push_audit_files(&ui, files);
            }
        });
    });
}

fn human_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    if n < KB {
        format!("{n} B")
    } else if n < MB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else if n < GB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else {
        format!("{:.2} GB", n as f64 / GB as f64)
    }
}

/// Push the current account's avatar (initials + palette) onto the UI.
/// Drives the left-rail avatar tile and the outgoing-message sender avatar
/// so they reflect the user's profile instead of a stale default.
fn set_my_avatar(ui: &DarkMatterLinux, backend: &Backend) {
    let my_id = backend.account().account_id_hex.clone();
    let label = my_avatar_label(backend, &my_id);
    let (a, b, init) = avatar_for(&label);
    ui.set_my_av_initials(s(&init));
    ui.set_my_av_a(a);
    ui.set_my_av_b(b);
    ui.set_my_display_name(s(&backend.account_display_name(&my_id)));
}

/// Compute and push rail badge counts from the chat list.
/// For now, chats badge counts pending chat requests (pending_confirmation).
fn set_rail_badges(ui: &DarkMatterLinux, chats: &ModelRc<ChatMeta>) {
    let mut chats_badge = 0;
    if let Some(vm) = chats.as_any().downcast_ref::<VecModel<ChatMeta>>() {
        for i in 0..vm.row_count() {
            if let Some(chat) = vm.row_data(i)
                && chat.is_chat_request
            {
                chats_badge += 1;
            }
        }
    }
    ui.set_rail_badge_chats(chats_badge);
    ui.set_rail_badge_contacts(0);
    ui.set_rail_badge_archive(0);
    ui.set_rail_badge_keys(0);
}

/// Clear one chat row's unread affordance in place (badge gone, read mark
/// restored). Used when a chat is opened so the badge disappears immediately,
/// ahead of the next full chat-list snapshot that recomputes it.
fn clear_chat_unread_row(ui: &DarkMatterLinux, idx: usize) {
    let chats = ui.get_chats();
    if let Some(vm) = chats.as_any().downcast_ref::<VecModel<ChatMeta>>()
        && let Some(mut row) = vm.row_data(idx)
        && !(row.badge.is_empty() && row.read)
    {
        row.badge = s("");
        row.read = true;
        vm.set_row_data(idx, row);
    }
}

/// String key used to derive the current account's avatar palette/initials.
/// Falls back to the account hex if no display name is available so the
/// avatar is at least deterministic per account.
fn my_avatar_label(backend: &Backend, my_id: &str) -> String {
    let name = backend.account_display_name(my_id);
    if name.is_empty() || name == "You" {
        my_id.to_string()
    } else {
        name
    }
}

/// One-time encryption-banner entrance when a chat is opened for the first time.
fn trigger_encryption_banner_entrance(
    ui: &DarkMatterLinux,
    chat_key: Option<&str>,
    banner_seen: &Arc<Mutex<std::collections::HashSet<String>>>,
) {
    let Some(chat_key) = chat_key else {
        ui.set_encryption_banner_first_show(false);
        return;
    };
    let first_show = {
        let mut seen = banner_seen.lock().unwrap();
        if seen.contains(chat_key) {
            false
        } else {
            seen.insert(chat_key.to_string());
            true
        }
    };
    ui.set_encryption_banner_first_show(first_show);
    if first_show {
        let weak = ui.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(520), move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_encryption_banner_first_show(false);
            }
        });
    }
}

/// Splash step index for a boot status line.
fn boot_phase_for_status(status: &str) -> i32 {
    if status.contains("Opening") {
        0
    } else if status.contains("Deriving") {
        1
    } else if status.contains("Publishing") {
        2
    } else {
        3
    }
}

/// Rasterize the named animal over an npub-derived gradient for immediate UI
/// preview (login mode 2). Must run on the UI thread — `slint::Image` is `!Send`.
fn local_animal_avatar_image(npub: &str, name: &str) -> Option<slint::Image> {
    let animal = name.rsplit(' ').next()?;
    let svg = animal_avatar::svg_for(animal)?;
    let (a, b, _) = avatar_for(npub);
    let png = animal_avatar::render_png(
        svg,
        (a.red(), a.green(), a.blue()),
        (b.red(), b.green(), b.blue()),
    )
    .ok()?;
    let img = image::load_from_memory(&png).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    let buffer =
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(img.as_raw(), w, h);
    Some(slint::Image::from_rgba8(buffer))
}

/// A random "[Adjective] [Animal]" display name for freshly generated
/// accounts, e.g. "Spooky Bear".
fn random_profile_name() -> String {
    const ADJECTIVES: &[&str] = &[
        "Spooky",
        "Cosmic",
        "Dapper",
        "Fuzzy",
        "Sleepy",
        "Sneaky",
        "Mighty",
        "Velvet",
        "Turbo",
        "Witty",
        "Zesty",
        "Plucky",
        "Quirky",
        "Nimble",
        "Frosty",
        "Mellow",
        "Peppy",
        "Rusty",
        "Stormy",
        "Sunny",
        "Dusty",
        "Misty",
        "Jolly",
        "Groovy",
        "Snazzy",
        "Breezy",
        "Cheeky",
        "Daring",
        "Electric",
        "Golden",
        "Icy",
        "Lucky",
        "Magnetic",
        "Neon",
        "Prickly",
        "Quantum",
        "Silent",
        "Vivid",
        "Wandering",
        "Wobbly",
    ];
    // Animals come from the SVG table so every name that can be generated is
    // guaranteed to have starter-avatar art.
    let animals = animal_avatar::ANIMAL_SVGS;
    let mut buf = [0u8; 8];
    // RNG failure shouldn't block account creation — buf stays zeroed and the
    // name degrades to a fixed (still valid) pick.
    let _ = getrandom::getrandom(&mut buf);
    let a = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize % ADJECTIVES.len();
    let n = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize % animals.len();
    format!("{} {}", ADJECTIVES[a], animals[n].0)
}

/// Publish a starter kind-0 profile with a random name for a freshly
/// generated account. Best-effort: a failure only logs — `on_published`
/// doesn't run, so the first-run pending-seed cell stays set and the
/// relays-added reboot retries — and the user can always set a name by hand.
/// Skips the publish when the directory already knows a profile for the
/// account (then it still counts as published).
fn publish_random_profile_async(
    backend: &Arc<Backend>,
    label: String,
    account_id_hex: String,
    preset_name: Option<String>,
    weak: slint::Weak<DarkMatterLinux>,
    on_published: impl FnOnce() + Send + 'static,
) {
    let backend = backend.clone();
    // The publish is a relay round-trip and `save_profile_for_label` blocks
    // on the backend runtime — worker thread, same as `on_save_profile`.
    std::thread::spawn(move || {
        if backend.cached_profile(&account_id_hex).is_some() {
            on_published();
            return;
        }
        let name = preset_name.unwrap_or_else(random_profile_name);
        // Companion art is best-effort: a render/upload miss just means a
        // name-only profile.
        let picture = seed_profile_picture(&backend, &label, &account_id_hex, &name);
        let profile = UserProfileMetadata {
            name: Some(name.clone()),
            display_name: Some(name.clone()),
            about: None,
            picture,
            nip05: None,
            lud16: None,
            created_at: 0,
            source_relays: Vec::new(),
        };
        match backend.save_profile_for_label(&label, profile) {
            Ok(_) => {
                eprintln!("[profile] seeded fresh account {label} as \"{name}\"");
                on_published();
                backend.refresh_profile_cache_async(&account_id_hex);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    populate_profile_async(&ui, &backend);
                });
            }
            Err(e) => {
                eprintln!("[profile] seeding starter profile for {label} failed: {e:#}")
            }
        }
    });
}

/// Render + upload the seeded profile's picture: the named animal's SVG over
/// a gradient derived from the account's npub. Returns the public Blossom
/// URL, or `None` on any failure (the caller publishes a name-only profile).
/// Blocks the calling thread on the upload — worker threads only.
fn seed_profile_picture(
    backend: &Arc<Backend>,
    label: &str,
    account_id_hex: &str,
    name: &str,
) -> Option<String> {
    let animal = name.rsplit(' ').next()?;
    let svg = animal_avatar::svg_for(animal)?;
    // The npub (not the raw hex) feeds the gradient hash, so the picture's
    // background is recognizably "theirs" wherever the npub is shown.
    let npub = npub_for_account_id(account_id_hex).ok()?;
    let (a, b, _) = avatar_for(&npub);
    let png = match animal_avatar::render_png(
        svg,
        (a.red(), a.green(), a.blue()),
        (b.red(), b.green(), b.blue()),
    ) {
        Ok(png) => png,
        Err(e) => {
            eprintln!("[profile] render starter avatar for {animal}: {e:#}");
            return None;
        }
    };
    let (tx, rx) = std::sync::mpsc::channel();
    backend.upload_public_blob_for_label_async(label, png, "image/png".to_string(), move |r| {
        let _ = tx.send(r);
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(Ok(url)) => Some(url),
        Ok(Err(e)) => {
            eprintln!("[profile] starter avatar upload failed: {e:#}");
            None
        }
        Err(_) => {
            eprintln!("[profile] starter avatar upload timed out");
            None
        }
    }
}

fn apply_profile(ui: &DarkMatterLinux, profile: Option<&UserProfileMetadata>) {
    let opt = |o: &Option<String>| o.clone().unwrap_or_default();
    match profile {
        Some(p) => {
            ui.set_profile_display_name(s(&opt(&p.display_name)));
            ui.set_profile_name(s(&opt(&p.name)));
            ui.set_profile_about(s(&opt(&p.about)));
            ui.set_profile_picture(s(&opt(&p.picture)));
            ui.set_profile_nip05(s(&opt(&p.nip05)));
            ui.set_profile_lud16(s(&opt(&p.lud16)));
        }
        None => {
            ui.set_profile_display_name(s(""));
            ui.set_profile_name(s(""));
            ui.set_profile_about(s(""));
            ui.set_profile_picture(s(""));
            ui.set_profile_nip05(s(""));
            ui.set_profile_lud16(s(""));
        }
    }
}

fn profile_from_ui(ui: &DarkMatterLinux) -> UserProfileMetadata {
    let opt = |s: SharedString| {
        let t = s.trim().to_string();
        if t.is_empty() { None } else { Some(t) }
    };
    UserProfileMetadata {
        name: opt(ui.get_profile_name()),
        display_name: opt(ui.get_profile_display_name()),
        about: opt(ui.get_profile_about()),
        picture: opt(ui.get_profile_picture()),
        nip05: opt(ui.get_profile_nip05()),
        lud16: opt(ui.get_profile_lud16()),
        created_at: 0,
        source_relays: Vec::new(),
    }
}

// ─── Backend ↔ UI bridge helpers ───────────────────────────────────────

/// Replace one row inside the outer chats-messages model. The outer model
/// holds `ModelRc<ChatMessage>` per chat; we swap in a fresh VecModel.
fn replace_message_row(outer: &ModelRc<ModelRc<ChatMessage>>, idx: usize, msgs: Vec<ChatMessage>) {
    let inner: ModelRc<ChatMessage> = ModelRc::new(VecModel::from(msgs));
    if let Some(vm) = outer
        .as_any()
        .downcast_ref::<VecModel<ModelRc<ChatMessage>>>()
        && idx < vm.row_count()
    {
        vm.set_row_data(idx, inner);
    }
}

/// Take a snapshot of chats and (lazily-loaded) messages, push them into the
/// Slint models, and store the parallel group-id list so on_send_message can
/// resolve the active group.
/// Repaint every surface that renders a timestamp — chat list, archived
/// list, and the open conversation. Called when the user flips the time or
/// date format so stale stamps don't linger until the next sync.
fn refresh_stamps_everywhere(
    ui: &DarkMatterLinux,
    backend_cell: &Arc<Mutex<Option<Arc<Backend>>>>,
    pending_state: &Arc<Mutex<PendingState>>,
    group_ids: &Arc<Mutex<Vec<String>>>,
    archived_group_ids: &Arc<Mutex<Vec<String>>>,
) {
    let guard = backend_cell.lock().unwrap();
    // Pre-login there's nothing on screen to repaint; boot applies the
    // formats before the first population.
    let Some(backend) = guard.as_ref() else {
        return;
    };
    refresh_chats_async(ui, backend, group_ids, |_, _, _| {});
    refresh_archived_async(ui, backend, archived_group_ids);
    let idx = ui.get_active_chat() as usize;
    if let Some(group_hex) = group_ids.lock().unwrap().get(idx).cloned() {
        // Window read on the backend runtime; the open conversation's stamps
        // repaint a beat later instead of stalling the UI thread on sqlite.
        let weak = ui.as_weak();
        let pending_state = pending_state.clone();
        let b = backend.clone();
        backend.tokio_handle().spawn(async move {
            let msgs = b
                .messages(&group_hex, Some(msg_window_for(&group_hex)))
                .unwrap_or_default();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else { return };
                let chats_messages = ui.get_chats_messages();
                let overlay = pending_state.lock().unwrap();
                rebuild_chat_messages_from(&b, &overlay, &chats_messages, idx, &group_hex, &msgs);
            });
        });
    }
}

/// Chat-list state gathered OFF the UI thread — every field is a sqlite
/// read (which can stall behind sync writes or a slow disk): the group
/// records, each group's latest message (preview/stamp), and the first
/// chat's message window (eagerly loaded so the default-selected chat
/// renders immediately).
struct ChatListSnapshot {
    records: Vec<AppGroupRecord>,
    /// Parallel to `records`.
    latest: Vec<Option<AppMessageRecord>>,
    /// Parallel to `records`: per-chat unread count at snapshot time.
    unread: Vec<u32>,
    first_msgs: Vec<AppMessageRecord>,
}

fn fetch_chat_list_snapshot(backend: &Backend) -> Option<ChatListSnapshot> {
    let records = match backend.chats() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[backend] chats snapshot failed: {e:#}");
            return None;
        }
    };
    let latest: Vec<Option<AppMessageRecord>> = records
        .iter()
        .map(|r| backend.latest_message(&r.group_id_hex))
        .collect();
    // Recompute unread for the full visible set from the authoritative read
    // markers. Clear first so chats that just left the visible set (archived,
    // blocked) drop out of the total; reseed/recount each one below.
    let state = unread_state();
    state.clear_counts();
    let my_id = backend.account().account_id_hex.clone();
    let now = now_unix_secs() as i64;
    let unread: Vec<u32> = records
        .iter()
        .zip(latest.iter())
        .map(|(r, lm)| {
            let marker = state.marker_or_seed(&r.group_id_hex, now);
            let n = count_unread(backend, &r.group_id_hex, &my_id, marker, lm.as_ref());
            state.set_count(&r.group_id_hex, n);
            n
        })
        .collect();
    let first_msgs = records
        .first()
        .map(|r| {
            backend
                .messages(&r.group_id_hex, Some(msg_window_for(&r.group_id_hex)))
                .unwrap_or_default()
        })
        .unwrap_or_default();
    Some(ChatListSnapshot {
        records,
        latest,
        unread,
        first_msgs,
    })
}

/// Fetch the chat-list snapshot on the backend runtime, apply it on the UI
/// thread (full `refresh_chats_from` + rail badges + avatar fetches), then
/// run `then` — still on the UI thread — for call-site follow-ups that need
/// the refreshed models/`group_ids` (e.g. selecting a freshly-created chat).
fn refresh_chats_async(
    ui: &DarkMatterLinux,
    backend: &Arc<Backend>,
    group_ids: &Arc<Mutex<Vec<String>>>,
    then: impl FnOnce(&DarkMatterLinux, &Arc<Backend>, &ChatListSnapshot) + Send + 'static,
) {
    let weak = ui.as_weak();
    let b = backend.clone();
    let group_ids = group_ids.clone();
    backend.tokio_handle().spawn(async move {
        let Some(snap) = fetch_chat_list_snapshot(&b) else {
            return;
        };
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            let chats = ui.get_chats();
            let chats_messages = ui.get_chats_messages();
            refresh_chats_from(&b, &snap, &chats, &chats_messages, &group_ids);
            set_rail_badges(&ui, &chats);
            refresh_unread_chrome(&ui);
            spawn_chat_list_avatar_fetches(&ui, &b);
            then(&ui, &b, &snap);
        });
    });
}

/// Async [`refresh_chats_from`] + [`refresh_archived_from`] + active-index
/// clamps — the post-mutation "refresh everything" used by accept / block /
/// archive / unarchive.
fn refresh_all_chat_models_async(
    ui: &DarkMatterLinux,
    backend: &Arc<Backend>,
    group_ids: &Arc<Mutex<Vec<String>>>,
    archived_group_ids: &Arc<Mutex<Vec<String>>>,
) {
    let weak = ui.as_weak();
    let b = backend.clone();
    let group_ids = group_ids.clone();
    let archived_group_ids = archived_group_ids.clone();
    backend.tokio_handle().spawn(async move {
        let chat_snap = fetch_chat_list_snapshot(&b);
        let archived_snap = fetch_archived_snapshot(&b);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            let chats = ui.get_chats();
            let archived = ui.get_archived_chats();
            if let Some(snap) = &chat_snap {
                let chats_messages = ui.get_chats_messages();
                refresh_chats_from(&b, snap, &chats, &chats_messages, &group_ids);
                set_rail_badges(&ui, &chats);
                refresh_unread_chrome(&ui);
                spawn_chat_list_avatar_fetches(&ui, &b);
            }
            if let Some(snap) = &archived_snap {
                refresh_archived_from(&b, snap, &archived, &archived_group_ids);
                spawn_archived_avatar_fetches(&ui, &b);
            }
            // Clamp active indices so we don't dangle past the end after a
            // row was removed.
            let len = chats.row_count() as i32;
            if ui.get_active_chat() >= len {
                ui.set_active_chat((len - 1).max(0));
            }
            let alen = archived.row_count() as i32;
            if ui.get_active_archived() >= alen {
                ui.set_active_archived((alen - 1).max(0));
            }
        });
    });
}

fn refresh_chats_from(
    backend: &Backend,
    snap: &ChatListSnapshot,
    chats: &ModelRc<ChatMeta>,
    chats_messages: &ModelRc<ModelRc<ChatMessage>>,
    group_ids: &Arc<Mutex<Vec<String>>>,
) {
    let records = &snap.records;
    eprintln!(
        "[refresh_chats] snapshot has {} records (archived flags: {:?})",
        records.len(),
        records.iter().map(|r| r.archived).collect::<Vec<_>>()
    );
    let my_id = backend.account().account_id_hex.clone();
    let my_label = my_avatar_label(backend, &my_id);

    // The latest message per group was prefetched with the snapshot so the
    // chat list shows real preview text + stamps instead of empty strings.
    let metas: Vec<ChatMeta> = records
        .iter()
        .zip(snap.latest.iter())
        .zip(snap.unread.iter())
        .map(|((r, latest), &unread)| {
            chat_meta_from(r, latest.as_ref(), &my_id, backend, unread)
        })
        .collect();
    let mut messages_outer: Vec<ModelRc<ChatMessage>> = Vec::with_capacity(records.len());
    let mut ids: Vec<String> = Vec::with_capacity(records.len());
    for record in records {
        ids.push(record.group_id_hex.clone());
        // Only the first chat's window was eagerly fetched; the others get
        // filled on selection. Keeps boot fast for users with many groups.
        let msgs: &[AppMessageRecord] = if messages_outer.is_empty() {
            &snap.first_msgs
        } else {
            &[]
        };
        let reactions = aggregate_reactions(msgs, &my_id);
        let edits = aggregate_edits(msgs);
        let profiles = build_sender_profiles(backend, msgs, &my_id);
        let is_group = backend.group_member_count(&record.group_id_hex) > 2;
        let by_id: HashMap<&str, &AppMessageRecord> = msgs
            .iter()
            .map(|m| (m.message_id_hex.as_str(), m))
            .collect();
        let row: Vec<ChatMessage> = msgs
            .iter()
            .filter(|m| is_visible_chat_message(m))
            .map(|m| {
                let r = reactions
                    .get(&m.message_id_hex)
                    .cloned()
                    .unwrap_or_default();
                let e = edits.get(&m.message_id_hex).cloned();
                chat_message_from_with_reactions(
                    m, &by_id, &my_id, &my_label, r, e, &profiles, is_group, false,
                )
            })
            .collect();
        messages_outer.push(ModelRc::new(VecModel::from(row)));
    }

    if let Some(vm) = chats.as_any().downcast_ref::<VecModel<ChatMeta>>() {
        vm.set_vec(metas);
    }
    if let Some(vm) = chats_messages
        .as_any()
        .downcast_ref::<VecModel<ModelRc<ChatMessage>>>()
    {
        vm.set_vec(messages_outer);
    }
    *group_ids.lock().unwrap() = ids;
}

/// Non-destructive chat-list refresh: update existing rows in place (keyed by
/// group id), append rows for groups we haven't seen, and leave the per-chat
/// message models alone. Used when boot's background relay sync completes —
/// a full [`refresh_chats`] would `set_vec` over the models, reorder
/// `group_ids`, and yank an already-open chat out from under the user.
/// Per-message updates are the live watchers' job; this only upgrades list
/// metadata (names/pictures resolved by the directory sync, previews from
/// caught-up messages).
/// Fetch the chat-list snapshot on the backend runtime, then apply a
/// non-destructive merge (+ rail badges + avatar fetches) on the UI thread.
fn merge_chat_list_rows_async(
    ui: &DarkMatterLinux,
    backend: &Arc<Backend>,
    group_ids: &Arc<Mutex<Vec<String>>>,
) {
    let weak = ui.as_weak();
    let b = backend.clone();
    let group_ids = group_ids.clone();
    backend.tokio_handle().spawn(async move {
        let Some(snap) = fetch_chat_list_snapshot(&b) else {
            return;
        };
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            let chats = ui.get_chats();
            let chats_messages = ui.get_chats_messages();
            merge_chat_list_rows_from(&b, &snap, &chats, &chats_messages, &group_ids);
            set_rail_badges(&ui, &chats);
            refresh_unread_chrome(&ui);
            spawn_chat_list_avatar_fetches(&ui, &b);
        });
    });
}

fn merge_chat_list_rows_from(
    backend: &Backend,
    snap: &ChatListSnapshot,
    chats: &ModelRc<ChatMeta>,
    chats_messages: &ModelRc<ModelRc<ChatMessage>>,
    group_ids: &Arc<Mutex<Vec<String>>>,
) {
    let my_id = backend.account().account_id_hex.clone();
    let Some(vm) = chats.as_any().downcast_ref::<VecModel<ChatMeta>>() else {
        return;
    };
    let mut ids = group_ids.lock().unwrap();
    for ((r, latest), &unread) in snap
        .records
        .iter()
        .zip(snap.latest.iter())
        .zip(snap.unread.iter())
    {
        let meta = chat_meta_from(r, latest.as_ref(), &my_id, backend, unread);
        if let Some(pos) = ids.iter().position(|g| g == &r.group_id_hex) {
            // Change-only: set_row_data dirties the row even when the data
            // is identical, and the post-sync merge touches EVERY row — the
            // all-rows flash was the visible glitch when a background sync
            // finished. Image handles are stable (thread-local cache), so
            // struct equality is meaningful here.
            let changed = vm.row_data(pos).map(|old| old != meta).unwrap_or(true);
            if changed {
                vm.set_row_data(pos, meta);
            }
        } else {
            ids.push(r.group_id_hex.clone());
            vm.push(meta);
            if let Some(mm) = chats_messages
                .as_any()
                .downcast_ref::<VecModel<ModelRc<ChatMessage>>>()
            {
                mm.push(ModelRc::new(VecModel::from(Vec::<ChatMessage>::new())));
            }
        }
    }
}

/// Spawn the chat-list watcher. New groups (welcomes, invites) get appended
/// to the chats model on the Slint thread.
///
/// The tokio callback can only capture Send data, so we hop into the Slint
/// event loop and look up the chat models off the UI handle from there.
/// Everything the UI needs (re)built when an account becomes active — shared
/// by the boot-success path and the account switcher. Every fetch runs on the
/// backend runtime and applies on the UI thread; nothing here blocks.
fn populate_models_for_active(
    ui: &DarkMatterLinux,
    backend: &Arc<Backend>,
    group_ids: &Arc<Mutex<Vec<String>>>,
    archived_group_ids: &Arc<Mutex<Vec<String>>>,
) {
    refresh_chats_async(ui, backend, group_ids, move |ui, b, snap| {
        // The first chat's extras (members panel, has-older, avatar fetches)
        // ride the chat-list continuation since they need the snapshot.
        if let Some(first) = snap.records.first() {
            push_group_members_to_ui_async(ui, b, &first.group_id_hex);
            ui.set_messages_has_older(snap.first_msgs.len() >= MESSAGE_WINDOW);
            spawn_message_avatar_fetches(ui, b, &snap.first_msgs);
        }
    });
    refresh_contacts_async(ui, backend, |_| {});
    refresh_archived_async(ui, backend, archived_group_ids);
    populate_profile_async(ui, backend);
    refresh_kp_local_async(ui, backend);
    refresh_network_post_boot(backend, ui);
    // Security & privacy flags live in marmot storage (disk) — read them on
    // the runtime too.
    {
        let weak = ui.as_weak();
        let b2 = backend.clone();
        backend.tokio_handle().spawn(async move {
            let telemetry = b2.telemetry_enabled();
            let audit = b2.audit_logs_enabled();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else { return };
                ui.set_telemetry_enabled(telemetry);
                ui.set_audit_enabled(audit);
            });
        });
    }
    refresh_audit_files(ui, backend);
}

/// Rebuild the account-switcher model: one row per local account. Names and
/// picture URLs resolve from the backend's profile cache on the runtime;
/// rows apply on the UI thread. Pictures not yet in the process-wide cache
/// are fetched once, then the model refreshes to pick them up.
fn refresh_accounts_model(ui: &DarkMatterLinux, backend: &Arc<Backend>) {
    let weak = ui.as_weak();
    let b = backend.clone();
    backend.tokio_handle().spawn(async move {
        let active_id = b.account().account_id_hex.to_ascii_lowercase();
        let rows: Vec<(String, String, Option<String>, String)> = b
            .accounts()
            .into_iter()
            .map(|a| {
                let id = a.account_id_hex.to_ascii_lowercase();
                let (name, pic) = b.account_name_and_picture(&id);
                let npub = npub_for_account_id(&id).unwrap_or_else(|_| id.clone());
                (id, name, pic, npub)
            })
            .collect();
        let b_for_fetch = b.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            let entries: Vec<AccountEntry> = rows
                .iter()
                .map(|(id, name, pic, npub)| {
                    // The active account's cache entry self-names as "You" —
                    // useless in a list of accounts; use the npub tail. Keep
                    // the avatar key consistent with `my_avatar_label`.
                    let unnamed = name.is_empty() || name == "You";
                    let display = if unnamed {
                        shorten_npub(npub)
                    } else {
                        name.clone()
                    };
                    let av_key = if unnamed { id.clone() } else { name.clone() };
                    let (col_a, col_b, init) = avatar_for(&av_key);
                    let (picture, has_picture) = bind_cached_picture(pic.as_deref());
                    AccountEntry {
                        id: s(id),
                        name: s(&display),
                        npub_short: s(&shorten_npub(npub)),
                        av_a: col_a,
                        av_b: col_b,
                        av_initials: s(&init),
                        picture,
                        has_picture,
                        active: *id == active_id,
                    }
                })
                .collect();
            ui.set_accounts(ModelRc::new(VecModel::from(entries)));

            let missing: Vec<String> = rows
                .iter()
                .filter_map(|(_, _, pic, _)| pic.as_deref())
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty() && !picture_cache_has(u))
                .collect();
            if missing.is_empty() {
                return;
            }
            let weak = ui.as_weak();
            let b = b_for_fetch.clone();
            b_for_fetch.tokio_handle().spawn(async move {
                let mut any_cached = false;
                for url in missing {
                    let Ok(resp) = reqwest::get(&url).await else {
                        continue;
                    };
                    let Ok(bytes) = resp.bytes().await else {
                        continue;
                    };
                    let Ok(pixels) = decode_avatar_pixels(&bytes) else {
                        continue;
                    };
                    picture_cache_put(url, pixels);
                    any_cached = true;
                }
                // Only re-run when something actually landed — a permanently
                // failing URL must not loop the refresh forever.
                if !any_cached {
                    return;
                }
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    refresh_accounts_model(&ui, &b);
                });
            });
        });
    });
}

/// Write one vault entry off the UI thread (every mutation re-seals the whole
/// map and rewrites the file). Best-effort: failures are logged, not surfaced.
fn vault_set_async(vault: &Arc<Mutex<Vault>>, key: String, value: String) {
    let vault = vault.clone();
    std::thread::spawn(move || {
        let mut v = vault.lock().unwrap();
        if let Err(e) = v.set(&key, &value) {
            eprintln!("[vault] set {key} failed: {e}");
        }
    });
}

/// User-facing message for a backup decrypt/read failure. A bad password is the
/// common case and gets its own clear line; everything else shows the detail.
fn import_backup_error(e: &backup::BackupError) -> String {
    match e {
        backup::BackupError::WrongPassword => "Wrong backup password.".to_string(),
        backup::BackupError::NotFound => "That backup file is gone.".to_string(),
        other => format!("Couldn't read backup: {other}"),
    }
}

/// Merge a set of nsecs (decrypted from an imported backup) into the running app.
/// Each nsec whose account isn't already present is re-logged via marmot — which
/// registers it and re-seals its secret into the active vault — and its bech32
/// backup is stored under `nsec:<hex>`. Already-present keys are counted as
/// skipped, not re-added. Runs on the UI thread; per-account completions hop
/// through a worker before the final summary lands back on the event loop.
fn merge_imported_accounts(
    ui: &DarkMatterLinux,
    backend: &Arc<Backend>,
    vault_cell: &Arc<Mutex<Option<Arc<Mutex<Vault>>>>>,
    nsecs: Vec<String>,
) {
    let existing: std::collections::BTreeSet<String> = backend
        .accounts()
        .into_iter()
        .map(|a| a.account_id_hex.to_ascii_lowercase())
        .collect();

    // (nsec, account-id-hex) for keys we don't already have.
    let mut to_add: Vec<(String, String)> = Vec::new();
    let mut skipped = 0usize;
    for nsec in nsecs {
        let Ok(keys) = Keys::parse(&nsec) else {
            continue;
        };
        let id = keys.public_key().to_hex().to_ascii_lowercase();
        if existing.contains(&id) || to_add.iter().any(|(_, e)| *e == id) {
            skipped += 1;
        } else {
            to_add.push((nsec, id));
        }
    }

    let total = to_add.len();
    if total == 0 {
        ui.set_import_backup_busy(false);
        ui.set_import_backup_status(if skipped == 0 {
            s("That backup holds no keys to import.")
        } else {
            format!("Nothing new — {skipped} account(s) already present.").into()
        });
        return;
    }

    // Shared tally across the per-account completions (each fires on a worker
    // thread). When the last one reports in, summarize on the UI thread.
    let done = Arc::new(Mutex::new((0usize, 0usize, 0usize))); // (ok, fail, done)
    let vault = vault_cell.lock().unwrap().clone();
    for (nsec, id) in to_add {
        let weak = ui.as_weak();
        let backend_final = backend.clone();
        let vault = vault.clone();
        let done = done.clone();
        backend.add_account_async(nsec.clone(), move |result| {
            // Seal the bech32 backup next to marmot's own secret so the boot
            // self-heal / export paths have it (mirrors the add-account flow).
            if result.is_ok()
                && let Some(vault) = vault.as_ref()
            {
                vault_set_async(vault, vault::nsec_key_for(&id), nsec.clone());
            }
            let finished = {
                let mut g = done.lock().unwrap();
                if result.is_ok() {
                    g.0 += 1;
                } else {
                    if let Err(e) = &result {
                        eprintln!("[import] add account {id} failed: {e:#}");
                    }
                    g.1 += 1;
                }
                g.2 += 1;
                g.2 == total
            };
            if !finished {
                return;
            }
            let (ok, fail) = {
                let g = done.lock().unwrap();
                (g.0, g.1)
            };
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else { return };
                ui.set_import_backup_busy(false);
                if ok > 0 {
                    refresh_accounts_model(&ui, &backend_final);
                }
                let mut msg = format!("Merged {ok} of {total} account(s).");
                if skipped > 0 {
                    msg.push_str(&format!(" {skipped} already present."));
                }
                if fail > 0 {
                    msg.push_str(&format!(" {fail} failed."));
                }
                ui.set_import_backup_status(msg.into());
                // Clean exit on a full success; otherwise keep the dialog open
                // so the partial-failure summary stays visible.
                if fail == 0 {
                    ui.set_show_import_backup(false);
                    ui.set_import_backup_password(s(""));
                }
            });
        });
    }
}

/// Clock-skew margin for the notification recency gate: a peer's clock can run
/// up to this many seconds behind ours and its genuinely-new message still
/// notifies. Wide enough for everyday NTP drift, narrow enough that the relay
/// backlog (messages authored long before this session) stays silent.
const NOTIF_SKEW_SECS: u64 = 120;

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Cap on how many recent messages a per-chat unread recount scans. Counts
/// above this saturate (the badge shows `99+` long before, anyway), so the
/// scan stays cheap even for a chat with a deep backlog.
const UNREAD_SCAN_CAP: usize = 200;

/// Process-wide unread state, lazily initialized from the persisted
/// `Settings::last_read` markers on first use. A `OnceLock` singleton (like
/// `active_group_slot`) rather than a threaded handle, because the chat watcher
/// and the chat-list snapshot fetch both run off the UI thread and would
/// otherwise need it plumbed through every refresh path.
fn unread_state() -> &'static unread::UnreadState {
    static UNREAD: std::sync::OnceLock<unread::UnreadState> = std::sync::OnceLock::new();
    UNREAD.get_or_init(|| {
        let markers: HashMap<String, i64> = Settings::load().last_read.into_iter().collect();
        unread::UnreadState::new(markers)
    })
}

/// Count a chat's unread messages relative to `marker`: incoming, visible chat
/// messages recorded after the marker. `latest` is the chat's most recent
/// message (already fetched by callers) — when it isn't newer than the marker
/// there's nothing unread, so the message scan is skipped entirely. That makes
/// the common case (an already-read chat) a single cheap comparison.
fn count_unread(
    backend: &Backend,
    group_hex: &str,
    my_id: &str,
    marker: i64,
    latest: Option<&AppMessageRecord>,
) -> u32 {
    match latest {
        Some(m) if m.recorded_at as i64 > marker => {
            let msgs = backend
                .messages(group_hex, Some(UNREAD_SCAN_CAP))
                .unwrap_or_default();
            msgs.iter()
                .filter(|m| {
                    m.recorded_at as i64 > marker
                        && !m.sender.eq_ignore_ascii_case(my_id)
                        && is_visible_chat_message(m)
                })
                .count() as u32
        }
        _ => 0,
    }
}

/// Push the aggregate unread total into the window title (the "tray" surface —
/// `(N) darkmatter`) and fold it into the rail's chats badge, which
/// `set_rail_badges` has just set from pending chat-requests. Call right after
/// `set_rail_badges` so the badge reflects unread + requests together.
fn refresh_unread_chrome(ui: &DarkMatterLinux) {
    let total = unread_state().total();
    ui.set_rail_badge_chats(ui.get_rail_badge_chats() + total as i32);
    ui.set_window_title(if total == 0 {
        s("darkmatter")
    } else {
        s(&format!("({total}) darkmatter"))
    });
}

/// Build the notification body for an incoming message. Group chats prefix the
/// sender's display name; 1:1 chats let the title (the peer's name) carry that.
/// These strings are Rust-side and intentionally not run through gettext (the
/// project keeps i18n to the Slint `@tr` catalogs).
fn notification_body(
    backend: &Backend,
    msg: &AppMessageRecord,
    group_hex: &str,
    preview: bool,
) -> String {
    if !preview {
        return "New message".to_string();
    }
    let (text, _) = split_effect_marker(&msg.plaintext);
    let text = text.trim();
    let text = if text.is_empty() {
        "Sent an attachment".to_string()
    } else {
        text.to_string()
    };
    if backend.group_member_count(group_hex) > 2 {
        format!("{}: {}", backend.account_display_name(&msg.sender), text)
    } else {
        text
    }
}

fn install_chat_watcher(
    backend: &Backend,
    weak: Weak<DarkMatterLinux>,
    group_ids: Arc<Mutex<Vec<String>>>,
    backend_cell: Arc<Mutex<Option<Arc<Backend>>>>,
    notif: Arc<notify::NotifState>,
    // Unix-seconds the watcher was installed at. Messages authored before this
    // (minus a skew margin) are treated as backlog and never notify — that's
    // what keeps the relay catch-up sync (and an account switch) from storming.
    since_secs: u64,
    watcher_cell: &Arc<Mutex<Option<JoinHandle<()>>>>,
) {
    let handle = backend.watch_chats(move |record| {
        let weak = weak.clone();
        let group_ids = group_ids.clone();
        let backend_cell = backend_cell.clone();
        let notif = notif.clone();
        // Recompute this chat's unread here, on the tokio thread, before hopping
        // to the UI: the count can scan up to `UNREAD_SCAN_CAP` decrypted
        // messages and must never run on the render thread. The UI closure below
        // zeroes it again if this turns out to be the chat on screen.
        let id = record.group_id_hex.clone();
        let now = now_unix_secs() as i64;
        let marker = unread_state().marker_or_seed(&id, now);
        let raw_unread = backend_cell
            .lock()
            .unwrap()
            .as_ref()
            .map(|b| {
                let my_id = b.account().account_id_hex.clone();
                let latest = b.latest_message(&id);
                count_unread(b, &id, &my_id, marker, latest.as_ref())
            })
            .unwrap_or(0);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            // The watcher fires on the tokio thread; the backend lives behind
            // `backend_cell`. Lock it here (on the UI thread) so chat-list rows
            // can resolve the 1:1 peer's profile name/picture. If it isn't
            // ready yet, fall back to the bare record (group name only).
            let guard = backend_cell.lock().unwrap();
            let chats = ui.get_chats();
            let chats_messages = ui.get_chats_messages();
            let id = record.group_id_hex.clone();
            // Group events (welcomes, evolutions) can change membership —
            // refresh this group's cached member list in the background so
            // cache-served reads (names, member panel, is-group flag) stay
            // current.
            if let Some(b) = guard.as_ref() {
                b.refresh_members_async(&id);
            }
            let my_id = guard
                .as_ref()
                .map(|b| b.account().account_id_hex.clone())
                .unwrap_or_default();
            // A message landing in the chat already on screen is read on
            // arrival: advance the marker and drop its unread to zero so an
            // open chat never grows a badge. A brand-new chat (not yet in
            // `group_ids`) reads as not-viewing.
            let viewing = ui.get_active_page() == Page::Chats as i32
                && group_ids
                    .lock()
                    .unwrap()
                    .get(ui.get_active_chat() as usize)
                    .map(|g| g == &id)
                    .unwrap_or(false);
            let unread = if viewing {
                unread_state().set_marker(&id, now);
                0
            } else {
                raw_unread
            };
            unread_state().set_count(&id, unread);
            let row_meta = match guard.as_deref() {
                Some(b) => chat_meta_from(&record, None, &my_id, b, unread),
                None => fallback_chat_meta(&record),
            };
            // Title for any notification below = the chat's display name.
            let chat_name = row_meta.name.to_string();
            let pos = {
                let mut ids = group_ids.lock().unwrap();
                if let Some(pos) = ids.iter().position(|g| g == &id) {
                    if let Some(vm) = chats.as_any().downcast_ref::<VecModel<ChatMeta>>() {
                        vm.set_row_data(pos, row_meta);
                    }
                    pos
                } else {
                    let pos = ids.len();
                    ids.push(id.clone());
                    if let Some(vm) = chats.as_any().downcast_ref::<VecModel<ChatMeta>>() {
                        vm.push(row_meta);
                    }
                    if let Some(vm) = chats_messages
                        .as_any()
                        .downcast_ref::<VecModel<ModelRc<ChatMessage>>>()
                    {
                        vm.push(ModelRc::new(VecModel::from(Vec::<ChatMessage>::new())));
                    }
                    pos
                }
            };
            let _ = pos;
            set_rail_badges(&ui, &chats);
            refresh_unread_chrome(&ui);

            // Desktop notification for a fresh incoming message in a chat the
            // user isn't currently viewing. Gates, in order: master toggle →
            // backend ready → there is a latest message → it's incoming →
            // recent enough (not backlog) → a visible chat message → its id
            // changed since we last saw this chat → not the on-screen chat.
            if notif.enabled.load(std::sync::atomic::Ordering::Relaxed)
                && let Some(b) = guard.as_ref()
                && let Some(m) = b.latest_message(&id)
            {
                let incoming = !m.sender.eq_ignore_ascii_case(&my_id);
                let recent = m.recorded_at.saturating_add(NOTIF_SKEW_SECS) >= since_secs;
                // `note_latest` runs before the `!viewing` check (it must record
                // the seen id even while viewing, so switching away later doesn't
                // re-notify); `&&` short-circuits the notification itself.
                if incoming
                    && recent
                    && !notif.is_muted(&id)
                    && is_visible_chat_message(&m)
                    && notif.note_latest(&id, &m.message_id_hex)
                    && !viewing
                {
                    let preview = notif.preview.load(std::sync::atomic::Ordering::Relaxed);
                    let sound = notif.sound.load(std::sync::atomic::Ordering::Relaxed);
                    let body = notification_body(b, &m, &id, preview);
                    // dbus IO — keep it off the UI thread.
                    std::thread::spawn(move || {
                        notify::show(&chat_name, &body, sound);
                    });
                }
            }
        });
    });
    // Replace (and stop) any watcher from a previously-active account.
    if let Some(old) = watcher_cell.lock().unwrap().replace(handle) {
        old.abort();
    }
}

// ─── marmot record → Slint struct converters ──────────────────────────

/// Minimal `ChatMeta` for when the backend isn't reachable (e.g. the chat
/// watcher fires before `backend_cell` is populated). Uses the MLS group name
/// only — no 1:1 peer resolution, no picture. The next full `refresh_chats`
/// upgrades the row with the real profile.
fn fallback_chat_meta(record: &AppGroupRecord) -> ChatMeta {
    let name = if record.profile.name.is_empty() {
        record.group_id_hex.clone()
    } else {
        record.profile.name.clone()
    };
    let (a, b, init) = avatar_for(&name);
    ChatMeta {
        name: s(&name),
        preview: s(&record.profile.description),
        stamp: s(""),
        last_seen: s(""),
        npub: s(&format!("mls:0x{}", short_hex(&record.group_id_hex))),
        session_time: s(""),
        badge: s(""),
        read: true,
        sending: false,
        av_a: a,
        av_b: b,
        av_initials: s(&init),
        picture: slint::Image::default(),
        has_picture: false,
        is_chat_request: record.pending_confirmation,
    }
}

fn chat_meta_from(
    record: &AppGroupRecord,
    last_message: Option<&AppMessageRecord>,
    my_account_id_hex: &str,
    backend: &Backend,
    unread: u32,
) -> ChatMeta {
    // 1:1 chats are named for the peer, not the (usually-empty) MLS group
    // profile — that's what made every direct chat read as a random hex. For
    // real group chats we keep the group profile name. The peer's picture is
    // bound from cache here and fetched lazily by the avatar worker.
    let peer = backend.direct_chat_peer(&record.group_id_hex);
    let (name, picture_url) = match &peer {
        Some(peer_id) => {
            let (peer_name, url) = backend.account_name_and_picture(peer_id);
            (peer_name, url)
        }
        None => {
            let group_name = if record.profile.name.is_empty() {
                record.group_id_hex.clone()
            } else {
                record.profile.name.clone()
            };
            (group_name, None)
        }
    };
    let (a, b, init) = avatar_for(&name);
    let (picture, has_picture) = bind_cached_picture(picture_url.as_deref());
    let (preview, stamp) = match last_message {
        Some(m) => {
            let mine = m.sender.eq_ignore_ascii_case(my_account_id_hex);
            let prefix = if mine {
                "You: ".to_string()
            } else {
                String::new()
            };
            (
                format!("{prefix}{}", m.plaintext),
                format_chat_stamp(m.recorded_at),
            )
        }
        None => (record.profile.description.clone(), String::new()),
    };
    ChatMeta {
        name: s(&name),
        preview: s(&preview),
        stamp: s(&stamp),
        last_seen: s(""),
        npub: s(&format!("mls:0x{}", short_hex(&record.group_id_hex))),
        session_time: s(""),
        badge: s(&unread::format_unread(unread)),
        // `read` drives the rail's sent-checkmark, shown only when there's no
        // unread badge competing for the slot — so a chat with unread hides it.
        read: unread == 0,
        sending: false,
        av_a: a,
        av_b: b,
        av_initials: s(&init),
        picture,
        has_picture,
        is_chat_request: record.pending_confirmation,
    }
}

/// Returns true when the record is a normal text message that belongs in
/// the visible bubble stream. Filters out everything marmot-app surfaces as
/// `AppMessageRecord` but isn't user-readable chat — push-token gossip
/// (MIP-05 kinds 447/448/449), reactions (kind 7), deletes (kind 5), agent
/// stream-start events (kind 1200), and anything else.
///
/// Reference: `crates/traits/src/app_event.rs` (MARMOT_APP_EVENT_KIND_*),
/// `spec/features/push-notifications.md` (kinds 447 / 448 / 449), and the
/// MIP-05 `{"v":"mip05-v1",…}` payload signature we saw on the wire.
fn is_visible_chat_message(record: &AppMessageRecord) -> bool {
    // Strict allow-list: only the chat kind. Reactions/deletes/streams/etc.
    // need their own renderers; until they have one, hide them rather than
    // dump raw JSON into the chat scroll.
    if record.kind != 9 {
        return false;
    }
    // Belt-and-suspenders: even if some other client is misbehaving and
    // shoving a token-gossip envelope into a kind-9 chat, filter it out by
    // signature.
    let t = record.plaintext.trim_start();
    if t.starts_with(r#"{"v":"mip05"#) || t.starts_with(r#"{"v": "mip05"#) {
        return false;
    }
    true
}

// Builds a confirmed message row; legitimately needs the full record context
// (record, id map, self identity/label, reactions, effect-gating), so the arg
// count exceeds clippy's default threshold.
#[allow(clippy::too_many_arguments)]
fn chat_message_from_with_reactions(
    record: &AppMessageRecord,
    records_by_id: &HashMap<&str, &AppMessageRecord>,
    my_account_id_hex: &str,
    my_label: &str,
    reactions: Vec<Reaction>,
    edit: Option<EditState>,
    profiles: &SenderProfiles,
    is_group: bool,
    // When true this build may start a one-shot effect burst (live arrival or
    // single-row refresh). Backfill passes false so opening a chat full of
    // effect-tagged history doesn't fire a burst storm.
    play_effects: bool,
) -> ChatMessage {
    let outgoing = record.sender.eq_ignore_ascii_case(my_account_id_hex);
    // Edited messages display the latest edit's text in place of the original;
    // the "(edited)" indicator + history modal expose the change. `can_edit`
    // gates the edit affordance to the author's own bubbles.
    let edited = edit.as_ref().map(|e| e.count > 0).unwrap_or(false);
    let edit_count = edit.as_ref().map(|e| e.count as i32).unwrap_or(0);
    let raw_display_text = edit
        .as_ref()
        .filter(|e| e.count > 0)
        .map(|e| e.text.as_str())
        .unwrap_or(record.plaintext.as_str());
    // Strip any trailing message-effect marker so it never reaches the bubble.
    // `effect_id` is the persistent identity (carried by every effect row so a
    // tap can replay it). `effect_autoplay` fires the burst by itself only on a
    // live incoming build — the sender already saw it on the optimistic row, and
    // backfill marks the id seen-but-quiet so it doesn't storm or replay later.
    let (display_text, body_effect) = split_effect_marker(raw_display_text);
    let effect_id = body_effect;
    let effect_autoplay =
        !outgoing && effect_should_autoplay(&record.message_id_hex, body_effect, play_effects);
    let (effect_clip_x, effect_clip_y) = effect_clip(effect_id)
        .map(|(x, y)| (x as i32, y as i32))
        .unwrap_or((0, 0));
    // Resolve the sender's directory profile (name + picture) so incoming rows
    // show a real identity rather than a hash of the raw pubkey. The lookup is
    // a cheap map hit — `profiles` was resolved once for the whole rebuild.
    // Outgoing rows key off the user's own label (matches the left-rail
    // avatar); their picture is painted by OutgoingRow via `my-picture`.
    let (sender_name, picture_url) = if outgoing {
        (my_label.to_string(), None)
    } else {
        profiles
            .get(record.sender.as_str())
            .cloned()
            .unwrap_or_else(|| (record.sender.clone(), None))
    };
    let key = if outgoing {
        my_label
    } else {
        sender_name.as_str()
    };
    let (a, b, init) = avatar_for(key);
    let (picture, has_picture) = bind_cached_picture(picture_url.as_deref());
    let (reply_id, reply_author, reply_text) =
        reply_preview_for(record, records_by_id, my_account_id_hex);
    let bubble_max = if outgoing { 440.0 } else { 560.0 };
    let lines = build_message_lines(display_text, bubble_max);

    // Attachment fields. Parse the NIP-92 `imeta` tags. Two or more image
    // attachments in one message render as an album grid; otherwise the first
    // reference drives the single chip/image-tile path.
    let all_refs = parse_all_media_references(&record.tags, record.source_epoch);
    let image_refs: Vec<MediaAttachmentReference> = all_refs
        .iter()
        .filter(|r| mime_is_image(&r.media_type))
        .cloned()
        .collect();
    let is_album = image_refs.len() >= 2;
    let (album, album_w, album_h) = if is_album {
        let (cells, w, h) = build_album_cells(&image_refs, &record.message_id_hex, outgoing);
        (ModelRc::new(VecModel::from(cells)), w, h)
    } else {
        no_album()
    };
    let media_ref = if is_album {
        None
    } else {
        all_refs.into_iter().next()
    };
    let (
        has_attachment,
        att_name,
        att_mime,
        att_size_label,
        att_is_image,
        att_image,
        att_has_image,
        att_loading,
    ) = match media_ref {
        Some(refp) => {
            let is_image = mime_is_image(&refp.media_type);
            let cached = if is_image {
                cached_attachment_image(&record.message_id_hex)
            } else {
                None
            };
            let (image, has_image) = match cached {
                Some(img) => (img, true),
                None => (slint::Image::default(), false),
            };
            let in_flight = attachment_in_flight()
                .lock()
                .map(|s| s.contains(&record.message_id_hex))
                .unwrap_or(false);
            (
                true,
                refp.file_name.clone(),
                refp.media_type.clone(),
                String::new(),
                is_image,
                image,
                has_image,
                in_flight,
            )
        }
        None => (
            false,
            String::new(),
            String::new(),
            String::new(),
            false,
            slint::Image::default(),
            false,
            false,
        ),
    };

    // Video attachment: not an image, so the tuple above left the poster empty.
    // The poster (decoded first frame, captured on first play) lives under a
    // distinct cache key; the duration label is captured alongside it.
    let att_is_video = has_attachment && mime_is_video(&att_mime);
    let (att_image, att_has_image) = if att_is_video {
        match attachment_image_cache_get(&vidposter_key(&record.message_id_hex)) {
            Some(px) => (image_from_pixels(&px), true),
            None => (att_image, att_has_image),
        }
    } else {
        (att_image, att_has_image)
    };
    // Audio attachment: inline player state. Progress + duration are cached
    // once the clip has been played; before that the duration is empty.
    let att_is_audio = has_attachment && !att_is_video && mime_is_audio(&att_mime);
    let (att_audio_playing, att_audio_progress) = if att_is_audio {
        let playing = current_audio_message_id()
            .lock()
            .unwrap()
            .as_ref()
            .map(|id| id == &record.message_id_hex)
            .unwrap_or(false)
            && with_active_player(|p| {
                p.as_ref()
                    .map(|player| player.state().playing)
                    .unwrap_or(false)
            });
        let progress = audio_progress()
            .lock()
            .unwrap()
            .get(&record.message_id_hex)
            .copied()
            .unwrap_or(0.0);
        (playing, progress)
    } else {
        (false, 0.0)
    };
    let att_duration = if att_is_video {
        video_duration_label(&record.message_id_hex)
    } else if att_is_audio {
        audio_meta()
            .lock()
            .unwrap()
            .get(&record.message_id_hex)
            .cloned()
            .unwrap_or_default()
    } else {
        String::new()
    };

    // Jumbo only for a bare emoji body — a reply/attachment/album wants its
    // normal bubble chrome around the block.
    let jumbo_emoji =
        !has_attachment && !is_album && reply_id.is_empty() && jumbo_emoji_count(display_text) > 0;

    ChatMessage {
        text: s(display_text),
        lines,
        jumbo_emoji,
        stamp: s(&format_unix(record.recorded_at)),
        outgoing,
        edited,
        edit_count,
        can_edit: outgoing,
        show_avatar: true,
        av_initials: s(&init),
        av_a: a,
        av_b: b,
        sender_id: s(&record.sender),
        sender_name: s(if outgoing { "" } else { &sender_name }),
        show_sender_name: is_group && !outgoing,
        picture,
        has_picture,
        bubble_max,
        gap_before: 0.0,
        first_in_group: true,
        last_in_group: true,
        message_id: s(&record.message_id_hex),
        reactions: ModelRc::new(VecModel::from(reactions)),
        pending: false,
        failed: false,
        reply_to_id: s(&reply_id),
        reply_to_text: s(&reply_text),
        reply_to_author: s(&reply_author),
        has_attachment,
        album,
        album_w,
        album_h,
        att_name: s(&att_name),
        att_mime: s(&att_mime),
        att_size_label: s(&att_size_label),
        att_is_image,
        att_is_video,
        att_is_audio,
        att_audio_playing,
        att_audio_progress,
        att_duration: s(&att_duration),
        att_image,
        att_has_image,
        att_loading,
        att_failed: false,
        effect_id,
        effect_clip_x,
        effect_clip_y,
        effect_autoplay,
    }
}

/// Resolve a record's reply target into (parent_id, author_label, preview).
/// Returns empty strings when the record isn't a reply. The author label is
/// "You" for your own messages and the parent's avatar-initials otherwise —
/// matches what the bubble's quoted-block expects to render.
fn reply_preview_for(
    record: &AppMessageRecord,
    records_by_id: &HashMap<&str, &AppMessageRecord>,
    my_account_id_hex: &str,
) -> (String, String, String) {
    // Marmot replies carry both `q` (quote-ref) and `e` (event-ref). Prefer
    // `q` since `e` may also be present on non-reply kinds.
    let parent_id = record
        .tags
        .iter()
        .find(|t| t.len() >= 2 && t[0] == "q")
        .or_else(|| record.tags.iter().find(|t| t.len() >= 2 && t[0] == "e"))
        .map(|t| t[1].clone());
    let Some(parent_id) = parent_id else {
        return (String::new(), String::new(), String::new());
    };
    // Parent might be out of the loaded slice — show a graceful placeholder
    // rather than nothing, since the row itself still reads as a reply.
    let parent = records_by_id.get(parent_id.as_str()).copied();
    let (author, preview) = match parent {
        Some(p) => {
            let author = if p.sender.eq_ignore_ascii_case(my_account_id_hex) {
                "You".to_string()
            } else {
                avatar_for(&p.sender).2
            };
            (
                author,
                truncate_preview(split_effect_marker(&p.plaintext).0, 160),
            )
        }
        None => (String::new(), String::new()),
    };
    (parent_id, author, preview)
}

/// Single-line, length-capped quote preview. Newlines collapse to spaces and
/// the result is ellipsized so long parent messages fit the chip + bubble
/// block without forcing a multi-line layout.
fn truncate_preview(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let flat = flat.trim();
    if flat.chars().count() <= max {
        flat.to_string()
    } else {
        let mut out: String = flat.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// Build the placeholder bubble for a not-yet-confirmed outgoing message.
/// The empty `message_id` suppresses the reactions row (you can't react to
/// something that doesn't exist on the wire yet), and the `pending`/`failed`
/// flags drive the bubble's dimming + indicator.
fn pending_chat_message(
    pending: &PendingSend,
    my_account_id_hex: &str,
    my_label: &str,
) -> ChatMessage {
    let (a, b, init) = avatar_for(my_label);
    // Pending rows replace the timestamp with status text — "sending…" while
    // we wait for the relay ack, or the failure pill once the send errored.
    // The bubble component handles the retry-affordance copy itself.
    let stamp = if pending.failed {
        "failed".to_string()
    } else {
        "sending…".to_string()
    };
    let (reply_id, reply_author, reply_text) = pending.reply_to.clone().unwrap_or_default();
    let bubble_max = 440.0_f32;
    let lines = build_message_lines(&pending.text, bubble_max);

    // Armed effect: `effect_id` is the persistent identity (so the row is
    // tap-to-replay), `effect_autoplay` fires it once on the optimistic row (a
    // failed send doesn't celebrate). pending.text is already clean — the marker
    // is only appended to the wire body, never stored here.
    let effect_id = pending.effect;
    let effect_autoplay =
        !pending.failed && effect_should_autoplay(&pending.temp_id, pending.effect, true);
    let (effect_clip_x, effect_clip_y) = effect_clip(effect_id)
        .map(|(x, y)| (x as i32, y as i32))
        .unwrap_or((0, 0));

    // Pending media optimistic-render. While the upload is in flight we render
    // the chip / image preview / album grid straight from the local bytes the
    // user picked, so the bubble doesn't pop in once the real record lands.
    let is_album = pending.media.len() >= 2;
    let (album, album_w, album_h) = if is_album {
        let (cells, w, h) = pending_album_cells(&pending.media, &pending.temp_id, true);
        (ModelRc::new(VecModel::from(cells)), w, h)
    } else {
        no_album()
    };
    let (
        has_attachment,
        att_name,
        att_mime,
        att_size_label,
        att_is_image,
        att_image,
        att_has_image,
        att_loading,
    ) = match (is_album, pending.media.first()) {
        (false, Some(m)) => {
            let (image, has_image) = match &m.local_preview {
                Some(p) => (image_from_pixels(p), true),
                None => (slint::Image::default(), false),
            };
            (
                true,
                m.file_name.clone(),
                m.media_type.clone(),
                human_bytes(m.size_bytes),
                m.is_image,
                image,
                has_image,
                !pending.failed,
            )
        }
        _ => (
            false,
            String::new(),
            String::new(),
            String::new(),
            false,
            slint::Image::default(),
            false,
            false,
        ),
    };

    // Optimistic video / audio bubble flags.
    let att_is_video = has_attachment && pending.media.first().map(|m| m.is_video).unwrap_or(false);
    let att_is_audio = has_attachment
        && !att_is_video
        && pending.media.first().map(|m| m.is_audio).unwrap_or(false);

    let jumbo_emoji =
        !has_attachment && !is_album && reply_id.is_empty() && jumbo_emoji_count(&pending.text) > 0;

    ChatMessage {
        text: s(&pending.text),
        lines,
        jumbo_emoji,
        stamp: s(&stamp),
        outgoing: true,
        edited: false,
        edit_count: 0,
        can_edit: false,
        show_avatar: true,
        av_initials: s(&init),
        av_a: a,
        av_b: b,
        // Pending rows are always the user's own outgoing message: no sender
        // label, and the outgoing avatar picture comes from `my-picture`.
        sender_id: s(my_account_id_hex),
        sender_name: s(""),
        show_sender_name: false,
        picture: slint::Image::default(),
        has_picture: false,
        bubble_max,
        gap_before: 0.0,
        first_in_group: true,
        last_in_group: true,
        // Carry the temp_id in `message_id` so the retry callback can find
        // the entry. The visual layer keys off `pending`/`failed`, not on
        // the id string being empty.
        message_id: s(&pending.temp_id),
        reactions: ModelRc::new(VecModel::from(Vec::<Reaction>::new())),
        pending: !pending.failed,
        failed: pending.failed,
        reply_to_id: s(&reply_id),
        reply_to_text: s(&reply_text),
        reply_to_author: s(&reply_author),
        has_attachment,
        album,
        album_w,
        album_h,
        att_name: s(&att_name),
        att_mime: s(&att_mime),
        att_size_label: s(&att_size_label),
        att_is_image,
        att_is_video,
        att_is_audio,
        att_audio_playing: false,
        att_audio_progress: 0.0,
        att_duration: s(""),
        att_image,
        att_has_image,
        att_loading,
        att_failed: pending.failed && has_attachment,
        effect_id,
        effect_clip_x,
        effect_clip_y,
        effect_autoplay,
    }
}

/// Apply the pending-reactions overlay onto an already-aggregated map.
/// Called after `aggregate_reactions` so optimistic clicks are visible
/// before the relay echoes the kind-7 event back.
fn apply_reaction_overlay(
    aggregate: &mut HashMap<String, Vec<Reaction>>,
    group_hex: &str,
    overlay: &PendingState,
) {
    for ((g, target), op) in &overlay.reactions {
        if g != group_hex {
            continue;
        }
        let entry = aggregate.entry(target.clone()).or_default();
        match op {
            PendingReactionOp::Add(emoji) => {
                // If the snapshot already shows my reaction with this emoji,
                // the overlay is redundant — the real record beat us here.
                let already_mine = entry.iter().any(|r| r.mine && r.emoji.as_str() == emoji);
                if already_mine {
                    continue;
                }
                if let Some(chip) = entry.iter_mut().find(|r| r.emoji.as_str() == emoji) {
                    if !chip.mine {
                        chip.count += 1;
                        chip.mine = true;
                    }
                } else {
                    entry.push(Reaction {
                        emoji: s(emoji),
                        count: 1,
                        mine: true,
                    });
                }
            }
            PendingReactionOp::Remove => {
                for chip in entry.iter_mut() {
                    if chip.mine {
                        chip.count = (chip.count - 1).max(0);
                        chip.mine = false;
                    }
                }
                entry.retain(|r| r.count > 0);
            }
        }
        // Re-sort: most-used first, ties broken by emoji.
        entry.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then(a.emoji.to_string().cmp(&b.emoji.to_string()))
        });
    }
}

// ─── Surgical row updates ─────────────────────────────────────────────
//
// Full `rebuild_chat_messages` calls were causing every bubble to remount
// (the inner VecModel got replaced wholesale), which re-fired the
// `init=>enter` fade on every neighbour. These helpers update just the
// affected row(s) so siblings stay put.
//
// Used by:
//   • send-ack reconciliation (pending row → confirmed row)
//   • react/unreact (target row gets new reactions)
//   • watcher kind-9 (append the new row)
//   • watcher kind-7/5 (refresh the target row's reactions)
//
// `rebuild_chat_messages` is still the right tool for "open fresh" cases:
// initial chat load and chat switching.

/// Apply an optimistic reaction op directly to the row already in the
/// model — no backend snapshot read, no re-aggregation. The clicked emoji
/// either bumps an existing chip's count + `mine` flag, or appears as a new
/// chip. Removal flips `mine` off and decrements; chips with count == 0 drop.
///
/// This is the hot path for emoji clicks; doing it model-only is what keeps
/// the picker feeling snappy when there are hundreds of messages in scope.
fn apply_reaction_to_model_row(
    chats_messages: &ModelRc<ModelRc<ChatMessage>>,
    idx: usize,
    target_id: &str,
    op: &PendingReactionOp,
) {
    let _ = with_inner_messages(chats_messages, idx, |vm| {
        let Some(pos) = find_message_row(vm, target_id) else {
            return;
        };
        let Some(mut row) = vm.row_data(pos) else {
            return;
        };
        let mut chips: Vec<Reaction> = (0..row.reactions.row_count())
            .filter_map(|i| row.reactions.row_data(i))
            .collect();
        match op {
            PendingReactionOp::Add(emoji) => {
                if let Some(chip) = chips.iter_mut().find(|c| c.emoji.as_str() == emoji) {
                    if !chip.mine {
                        chip.count += 1;
                        chip.mine = true;
                    }
                } else {
                    chips.push(Reaction {
                        emoji: s(emoji),
                        count: 1,
                        mine: true,
                    });
                }
            }
            PendingReactionOp::Remove => {
                for chip in chips.iter_mut() {
                    if chip.mine {
                        chip.count = (chip.count - 1).max(0);
                        chip.mine = false;
                    }
                }
                chips.retain(|c| c.count > 0);
            }
        }
        chips.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then(a.emoji.to_string().cmp(&b.emoji.to_string()))
        });
        row.reactions = ModelRc::new(VecModel::from(chips));
        vm.set_row_data(pos, row);
    });
}

/// Surgically rewrite one bubble's body to `new_text` and flag it edited.
/// The optimistic counterpart to [`apply_reaction_to_model_row`] — used the
/// instant the user confirms an edit, before the kind-1009 echoes back.
fn apply_edit_to_model_row(
    chats_messages: &ModelRc<ModelRc<ChatMessage>>,
    idx: usize,
    target_id: &str,
    new_text: &str,
) {
    let _ = with_inner_messages(chats_messages, idx, |vm| {
        let Some(pos) = find_message_row(vm, target_id) else {
            return;
        };
        let Some(mut row) = vm.row_data(pos) else {
            return;
        };
        row.text = s(new_text);
        row.lines = build_message_lines(new_text, row.bubble_max);
        row.jumbo_emoji = !row.has_attachment
            && row.album.row_count() == 0
            && row.reply_to_id.is_empty()
            && jumbo_emoji_count(new_text) > 0;
        row.edited = true;
        row.edit_count += 1;
        vm.set_row_data(pos, row);
    });
}

/// Surgically refresh one bubble (by message id) from a prefetched snapshot +
/// overlay. Used by react/unreact and the kind-7/5 echo handler — they all
/// only need to touch the target row, not the whole model. `all` must be the
/// current message window for `group_hex`, read OFF the UI thread (sqlite can
/// stall behind sync writes or a slow disk); use
/// [`refresh_one_message_row_async`] when you don't already hold a snapshot.
fn refresh_one_message_row_from(
    backend: &Backend,
    overlay: &PendingState,
    chats_messages: &ModelRc<ModelRc<ChatMessage>>,
    idx: usize,
    group_hex: &str,
    target_id: &str,
    all: &[AppMessageRecord],
) {
    let my_id = backend.account().account_id_hex.clone();
    let my_label = my_avatar_label(backend, &my_id);
    let Some(rec) = all.iter().find(|m| m.message_id_hex == target_id).cloned() else {
        return;
    };
    let mut row = build_one_message_row(&rec, all, &my_id, &my_label, group_hex, overlay, backend);
    with_inner_messages(chats_messages, idx, |vm| {
        if let Some(pos) = find_message_row(vm, target_id) {
            preserve_grouping_flags(vm, pos, &mut row);
            vm.set_row_data(pos, row);
        }
    });
}

/// Read the message window for `group_hex` on the backend runtime, then hop
/// to the event loop and surgically refresh `target_id`'s bubble. Never
/// blocks the caller — safe from any thread, including Slint callbacks.
/// The chat index is re-resolved from `group_ids` at apply time so a chat
/// list that moved underneath the round-trip still lands the row in the
/// right slot.
fn refresh_one_message_row_async(
    backend: &Arc<Backend>,
    weak: Weak<DarkMatterLinux>,
    pending_state: Arc<Mutex<PendingState>>,
    group_ids: Arc<Mutex<Vec<String>>>,
    group_hex: String,
    target_id: String,
) {
    let b = backend.clone();
    backend.tokio_handle().spawn(async move {
        let all = b
            .messages(&group_hex, Some(msg_window_for(&group_hex)))
            .unwrap_or_default();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            let ids = group_ids.lock().unwrap();
            let Some(idx) = ids.iter().position(|g| g == &group_hex) else {
                return;
            };
            drop(ids);
            let overlay = pending_state.lock().unwrap();
            let chats_messages = ui.get_chats_messages();
            refresh_one_message_row_from(
                &b,
                &overlay,
                &chats_messages,
                idx,
                &group_hex,
                &target_id,
                &all,
            );
        });
    });
}

/// Run `f` against the inner `VecModel<ChatMessage>` for a chat slot.
/// Returns `None` if the model/index isn't shaped like we expect.
fn with_inner_messages<R>(
    chats_messages: &ModelRc<ModelRc<ChatMessage>>,
    idx: usize,
    f: impl FnOnce(&VecModel<ChatMessage>) -> R,
) -> Option<R> {
    let outer = chats_messages
        .as_any()
        .downcast_ref::<VecModel<ModelRc<ChatMessage>>>()?;
    let inner = outer.row_data(idx)?;
    let vm = inner.as_any().downcast_ref::<VecModel<ChatMessage>>()?;
    Some(f(vm))
}

/// Find the index of the row whose `message_id` matches `id`.
fn find_message_row(vm: &VecModel<ChatMessage>, id: &str) -> Option<usize> {
    (0..vm.row_count()).find(|&i| {
        vm.row_data(i)
            .map(|r| r.message_id.as_str() == id)
            .unwrap_or(false)
    })
}

/// Build the `ChatMessage` for a single record, applying any pending-reaction
/// overlay so optimistic chips show up the moment the user clicks them.
fn build_one_message_row(
    record: &AppMessageRecord,
    all_records: &[AppMessageRecord],
    my_id: &str,
    my_label: &str,
    group_hex: &str,
    overlay: &PendingState,
    backend: &Backend,
) -> ChatMessage {
    maybe_autoload_album(group_hex, record);
    let mut reactions = aggregate_reactions(all_records, my_id);
    apply_reaction_overlay(&mut reactions, group_hex, overlay);
    let r = reactions
        .get(&record.message_id_hex)
        .cloned()
        .unwrap_or_default();
    let mut edits = aggregate_edits(all_records);
    apply_edit_overlay(&mut edits, group_hex, overlay);
    let e = edits.get(&record.message_id_hex).cloned();
    // Resolve just this record's sender (single-row refresh path).
    let profiles = build_sender_profiles(backend, std::slice::from_ref(record), my_id);
    let is_group = backend.group_member_count(group_hex) > 2;
    let by_id: HashMap<&str, &AppMessageRecord> = all_records
        .iter()
        .map(|m| (m.message_id_hex.as_str(), m))
        .collect();
    chat_message_from_with_reactions(
        record, &by_id, my_id, my_label, r, e, &profiles, is_group, true,
    )
}

/// Rebuild one chat's message row from `(backend snapshot ∪ pending overlay)`.
/// This is the single source of truth — every code path that mutates state
/// (send, react, unreact, watcher fires) ends here.
/// Consecutive messages from the same sender within this many seconds collapse
/// into one visual group: a single trailing avatar, one name label, tightened
/// corners, and no inter-bubble gap.
const GROUP_WINDOW_SECS: u64 = 5 * 60;

/// A grouping key: (sender_lowercased, is_outgoing, recorded_at_secs).
type GroupKey = (String, bool, u64);

fn keys_grouped(a: &GroupKey, b: &GroupKey) -> bool {
    a.1 == b.1 && a.0 == b.0 && a.2.abs_diff(b.2) <= GROUP_WINDOW_SECS
}

/// Stamp first/last/avatar/name/gap grouping flags onto a freshly-built run of
/// rows. `keys` must be in the same order and length as `rows`.
fn apply_grouping(rows: &mut [ChatMessage], keys: &[GroupKey]) {
    let n = rows.len();
    for i in 0..n {
        let first = i == 0 || !keys_grouped(&keys[i - 1], &keys[i]);
        let last = i + 1 == n || !keys_grouped(&keys[i], &keys[i + 1]);
        rows[i].first_in_group = first;
        rows[i].last_in_group = last;
        // Avatar rides the bottom of a stack; the name label tops it.
        rows[i].show_avatar = last;
        rows[i].show_sender_name = rows[i].show_sender_name && first;
        rows[i].gap_before = if first && i != 0 { 10.0 } else { 0.0 };
    }
}

/// Build the grouping keys for a chat in display order: visible records first,
/// then any pending sends (which are always my own, appended at the end).
fn grouping_keys(msgs: &[AppMessageRecord], my_id: &str, pending_count: usize) -> Vec<GroupKey> {
    let mut keys: Vec<GroupKey> = msgs
        .iter()
        .filter(|m| is_visible_chat_message(m))
        .map(|m| {
            (
                m.sender.to_ascii_lowercase(),
                m.sender.eq_ignore_ascii_case(my_id),
                m.recorded_at,
            )
        })
        .collect();
    // Pending rows inherit the latest timestamp so they group with the most
    // recent confirmed run from me.
    let pend_t = keys.last().map(|k| k.2).unwrap_or(0);
    for _ in 0..pending_count {
        keys.push((my_id.to_ascii_lowercase(), true, pend_t));
    }
    keys
}

/// Append `row` to the chat model, folding it into the previous row's visual
/// group when they share sender + direction. Recomputes the new row's grouping
/// flags and clears the previous row's avatar/tail so live arrivals stack the
/// same way a full rebuild would.
fn push_message_grouped(vm: &VecModel<ChatMessage>, mut row: ChatMessage) {
    let n = vm.row_count();
    let mut grouped = false;
    if n > 0
        && let Some(mut prev) = vm.row_data(n - 1)
    {
        let same = (row.outgoing && prev.outgoing)
            || (!row.outgoing
                && !prev.outgoing
                && !row.sender_id.is_empty()
                && prev
                    .sender_id
                    .as_str()
                    .eq_ignore_ascii_case(row.sender_id.as_str()));
        if same {
            grouped = true;
            prev.last_in_group = false;
            prev.show_avatar = false;
            vm.set_row_data(n - 1, prev);
        }
    }
    row.first_in_group = !grouped;
    row.last_in_group = true;
    row.show_avatar = true;
    if grouped {
        row.show_sender_name = false;
        row.gap_before = 0.0;
    } else {
        row.gap_before = if n > 0 { 10.0 } else { 0.0 };
    }
    vm.push(row);
}

/// Copy the grouping flags off the row currently at `pos` onto `row`. Used when
/// swapping a row in place (reaction refresh, send reconciliation) so a single-
/// row update doesn't reset that bubble's grouping to the standalone defaults.
fn preserve_grouping_flags(vm: &VecModel<ChatMessage>, pos: usize, row: &mut ChatMessage) {
    if let Some(old) = vm.row_data(pos) {
        row.first_in_group = old.first_in_group;
        row.last_in_group = old.last_in_group;
        row.show_avatar = old.show_avatar;
        row.show_sender_name = old.show_sender_name;
        row.gap_before = old.gap_before;
    }
}

/// Rebuild one chat's rows from a PREFETCHED window snapshot. `msgs` must be
/// read off the UI thread (see [`refresh_one_message_row_async`] for why);
/// the row building itself is pure CPU + cache lookups and stays on the UI
/// thread because rows hold `slint::Image` handles.
fn rebuild_chat_messages_from(
    backend: &Backend,
    pending: &PendingState,
    chats_messages: &ModelRc<ModelRc<ChatMessage>>,
    idx: usize,
    group_hex: &str,
    msgs: &[AppMessageRecord],
) {
    let t0 = std::time::Instant::now();
    let my_id = backend.account().account_id_hex.clone();
    let my_label = my_avatar_label(backend, &my_id);
    let t_label = t0.elapsed();
    let t_msgs = t0.elapsed();
    let mut reactions = aggregate_reactions(msgs, &my_id);
    apply_reaction_overlay(&mut reactions, group_hex, pending);
    let mut edits = aggregate_edits(msgs);
    apply_edit_overlay(&mut edits, group_hex, pending);
    let profiles = build_sender_profiles(backend, msgs, &my_id);
    let t_profiles = t0.elapsed();
    let is_group = backend.group_member_count(group_hex) > 2;
    let by_id: HashMap<&str, &AppMessageRecord> = msgs
        .iter()
        .map(|m| (m.message_id_hex.as_str(), m))
        .collect();

    let mut rows: Vec<ChatMessage> = msgs
        .iter()
        .filter(|m| is_visible_chat_message(m))
        .map(|m| {
            maybe_autoload_album(group_hex, m);
            let r = reactions
                .get(&m.message_id_hex)
                .cloned()
                .unwrap_or_default();
            let e = edits.get(&m.message_id_hex).cloned();
            chat_message_from_with_reactions(
                m, &by_id, &my_id, &my_label, r, e, &profiles, is_group, false,
            )
        })
        .collect();

    let pending_count = pending.sends.get(group_hex).map(|p| p.len()).unwrap_or(0);
    if let Some(pendings) = pending.sends.get(group_hex) {
        for p in pendings {
            rows.push(pending_chat_message(p, &my_id, &my_label));
        }
    }

    let keys = grouping_keys(msgs, &my_id, pending_count);
    apply_grouping(&mut rows, &keys);
    let t_rows = t0.elapsed();

    replace_message_row(chats_messages, idx, rows);
    eprintln!(
        "[switch-timing]   detail: label={t_label:?} msgs={:?} profiles={:?} rows={:?} replace={:?}",
        t_msgs - t_label,
        t_profiles - t_msgs,
        t_rows - t_profiles,
        t0.elapsed() - t_rows,
    );
}

/// Walk all message records and group kind-7 reactions by target id.
/// Returns a map from target message_id → ordered `Reaction` chips.
fn aggregate_reactions(
    records: &[AppMessageRecord],
    my_account_id_hex: &str,
) -> std::collections::HashMap<String, Vec<Reaction>> {
    use std::collections::HashMap;
    // target_id → (emoji → (count, mine))
    let mut by_target: HashMap<String, HashMap<String, (i32, bool)>> = HashMap::new();
    for r in records {
        if r.kind != 7 {
            continue;
        }
        // The first `e` tag points at the target. Skip if missing.
        let Some(target) = r
            .tags
            .iter()
            .find(|t| t.len() >= 2 && t[0] == "e")
            .map(|t| t[1].clone())
        else {
            continue;
        };
        let emoji = r.plaintext.trim().to_string();
        if emoji.is_empty() || emoji == "-" {
            continue;
        }
        let mine = r.sender.eq_ignore_ascii_case(my_account_id_hex);
        let entry = by_target
            .entry(target)
            .or_default()
            .entry(emoji)
            .or_insert((0, false));
        entry.0 += 1;
        entry.1 = entry.1 || mine;
    }
    by_target
        .into_iter()
        .map(|(target, emojis)| {
            let mut list: Vec<Reaction> = emojis
                .into_iter()
                .map(|(emoji, (count, mine))| Reaction {
                    emoji: s(&emoji),
                    count,
                    mine,
                })
                .collect();
            // Most-used first; deterministic tiebreak by emoji.
            list.sort_by(|a, b| {
                b.count
                    .cmp(&a.count)
                    .then(a.emoji.to_string().cmp(&b.emoji.to_string()))
            });
            (target, list)
        })
        .collect()
}

/// Resolved edit state for one message: the text to display (latest edit's
/// content) and how many edits have been applied. `count == 0` means the
/// message is unedited and the original `plaintext` should be shown.
#[derive(Clone, Default)]
struct EditState {
    text: String,
    count: usize,
}

/// Walk all records and resolve kind-1009 edits per target message.
///
/// Authorship is enforced here: an edit is only honored when its authenticated
/// author (the inner event's `sender`, which marmot guarantees equals the
/// MLS-authenticated sender) matches the *original* message's author. A
/// kind-1009 from anyone else referencing your message is ignored. Edits are
/// ordered by `(recorded_at, id)` and the newest wins as the displayed text.
fn aggregate_edits(records: &[AppMessageRecord]) -> std::collections::HashMap<String, EditState> {
    use std::collections::HashMap;
    // message_id → original author, for kind-9 chat messages only.
    let mut author_of: HashMap<&str, &str> = HashMap::new();
    for r in records {
        if r.kind == 9 {
            author_of.insert(r.message_id_hex.as_str(), r.sender.as_str());
        }
    }
    // target_id → ordered (recorded_at, id, content) edits.
    let mut by_target: HashMap<String, Vec<(u64, String, String)>> = HashMap::new();
    for r in records {
        if r.kind != 1009 {
            continue;
        }
        let Some(target) = r
            .tags
            .iter()
            .find(|t| t.len() >= 2 && t[0] == "e")
            .map(|t| t[1].as_str())
        else {
            continue;
        };
        // Only the original author may edit their own message.
        let Some(orig_author) = author_of.get(target) else {
            continue;
        };
        if !r.sender.eq_ignore_ascii_case(orig_author) {
            continue;
        }
        if r.plaintext.trim().is_empty() {
            continue;
        }
        by_target.entry(target.to_string()).or_default().push((
            r.recorded_at,
            r.message_id_hex.clone(),
            r.plaintext.clone(),
        ));
    }
    by_target
        .into_iter()
        .map(|(target, mut versions)| {
            versions.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
            let count = versions.len();
            let text = versions.pop().map(|v| v.2).unwrap_or_default();
            (target, EditState { text, count })
        })
        .collect()
}

/// Layer the pending-edit overlay onto an aggregated edit map, so an
/// optimistic edit shows before its kind-1009 echoes back. Mirrors
/// [`apply_reaction_overlay`].
fn apply_edit_overlay(
    aggregate: &mut std::collections::HashMap<String, EditState>,
    group_hex: &str,
    overlay: &PendingState,
) {
    for ((g, target), content) in &overlay.edits {
        if g != group_hex {
            continue;
        }
        let entry = aggregate.entry(target.clone()).or_default();
        entry.text = content.clone();
        entry.count += 1;
    }
}

/// Build the full version history (original + each edit, oldest→newest) for the
/// edit-history modal. Author-enforced, same as [`aggregate_edits`]. Returns an
/// empty vec when the message has no edits.
fn build_edit_history(records: &[AppMessageRecord], message_id: &str) -> Vec<EditVersion> {
    let Some(original) = records
        .iter()
        .find(|r| r.kind == 9 && r.message_id_hex == message_id)
    else {
        return Vec::new();
    };
    let mut edits: Vec<&AppMessageRecord> = records
        .iter()
        .filter(|r| r.kind == 1009)
        .filter(|r| r.sender.eq_ignore_ascii_case(&original.sender))
        .filter(|r| {
            r.tags
                .iter()
                .any(|t| t.len() >= 2 && t[0] == "e" && t[1] == message_id)
        })
        .filter(|r| !r.plaintext.trim().is_empty())
        .collect();
    if edits.is_empty() {
        return Vec::new();
    }
    edits.sort_by(|a, b| {
        a.recorded_at
            .cmp(&b.recorded_at)
            .then(a.message_id_hex.cmp(&b.message_id_hex))
    });
    let mut out = Vec::with_capacity(edits.len() + 1);
    out.push(EditVersion {
        label: s("Original"),
        text: s(&original.plaintext),
        stamp: s(&format_unix(original.recorded_at)),
    });
    for e in edits {
        out.push(EditVersion {
            label: s("Edited"),
            text: s(&e.plaintext),
            stamp: s(&format_unix(e.recorded_at)),
        });
    }
    out
}

/// Rasterize `text` into a QR code image. Black modules on an opaque white
/// field with a 4-module quiet zone baked in, so the code scans regardless of
/// the app theme behind it. Rendered at 3px/module so the native size stays
/// below the on-screen size — `image-rendering: pixelated` then only ever
/// upscales, which can't thin or drop module rows the way a nearest-neighbor
/// downscale can. Must run on the UI thread (`slint::Image` is `!Send`).
fn qr_image(text: &str) -> slint::Image {
    let Ok(code) = qrcode::QrCode::new(text.as_bytes()) else {
        return slint::Image::default();
    };
    const QUIET: usize = 4;
    const SCALE: usize = 3;
    let n = code.width();
    let side = (n + 2 * QUIET) * SCALE;
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(side as u32, side as u32);
    let px = buf.make_mut_slice();
    px.fill(slint::Rgba8Pixel {
        r: 255,
        g: 255,
        b: 255,
        a: 255,
    });
    let modules = code.to_colors();
    for y in 0..n {
        for x in 0..n {
            if modules[y * n + x] != qrcode::Color::Dark {
                continue;
            }
            let (x0, y0) = ((QUIET + x) * SCALE, (QUIET + y) * SCALE);
            for row in y0..y0 + SCALE {
                px[row * side + x0..row * side + x0 + SCALE].fill(slint::Rgba8Pixel {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 255,
                });
            }
        }
    }
    slint::Image::from_rgba8(buf)
}

/// Cheap deterministic avatar palette + initials from any string key.
fn avatar_for(key: &str) -> (Color, Color, String) {
    let mut h: u32 = 0x811c_9dc5;
    for b in key.as_bytes() {
        h = h.wrapping_mul(16_777_619) ^ (*b as u32);
    }
    let hue_a = 0x80_8080u32.wrapping_add(h & 0x7f_7f7f);
    let hue_b = 0x20_2020u32.wrapping_add(h.rotate_left(7) & 0x3f_3f3f);
    let init: String = key
        .split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase();
    let init = if init.is_empty() {
        key.chars().take(2).collect::<String>().to_uppercase()
    } else {
        init
    };
    (rgb(hue_a), rgb(hue_b), init)
}

fn short_hex(s: &str) -> String {
    if s.len() <= 6 {
        s.to_string()
    } else {
        s[..6].to_string()
    }
}

// ─── Emoji catalog ──────────────────────────────────────────────────────

// `emoji_sprite_map` and `EMOJI_SPRITE_PNG` come from dm-ui (via the glob
// import at the top) — they're emitted by that crate's build.rs.

// No cap: the picker grid in Slint manually virtualizes (only rows whose
// y-range intersects the viewport are instantiated), so the full ~1900-emoji
// catalog stays cheap regardless of how many match.

/// Decode the build-time sprite sheet into a `slint::Image`. Cached so
/// repeated calls reuse the same texture.
fn emoji_sprite_image() -> slint::Image {
    use std::cell::RefCell;
    thread_local! {
        static CACHE: RefCell<Option<slint::Image>> = const { RefCell::new(None) };
    }
    if let Some(img) = CACHE.with(|c| c.borrow().clone()) {
        return img;
    }
    let decoded = image::load_from_memory_with_format(EMOJI_SPRITE_PNG, image::ImageFormat::Png)
        .expect("decode embedded twemoji sprite")
        .to_rgba8();
    let (w, h) = decoded.dimensions();
    let buffer =
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(decoded.as_raw(), w, h);
    let image = slint::Image::from_rgba8(buffer);
    CACHE.with(|c| *c.borrow_mut() = Some(image.clone()));
    image
}

/// Build a fast lookup from emoji string → its (x, y) position in the
/// sprite sheet, using the table emitted by build.rs.
fn emoji_position_index() -> &'static std::collections::HashMap<&'static str, (u32, u32)> {
    use std::collections::HashMap;
    use std::sync::OnceLock;
    static IDX: OnceLock<HashMap<&'static str, (u32, u32)>> = OnceLock::new();
    IDX.get_or_init(|| {
        emoji_sprite_map::EMOJI_POSITIONS
            .iter()
            .map(|(e, x, y)| (*e, (*x, *y)))
            .collect()
    })
}

// ── Message effects (Telegram-style bursts) ─────────────────────────────────
//
// A small catalog of one-shot particle effects. Each is (catalog-id, wire-key,
// emoji). The id drives the Slint motion switch in message-effect-layer.slint;
// the wire-key is what travels in the body marker; the emoji is rendered as the
// flying particle (resolved to a sprite-sheet tile via the inline-emoji index).
const EFFECTS: &[(i32, &str, &str)] = &[
    (1, "love", "❤️"),
    (2, "fire", "🔥"),
    (3, "party", "🎉"),
    (4, "star", "⭐"),
    (5, "like", "👍"),
];

/// Invisible delimiter wrapping the body effect marker. U+2063 (INVISIBLE
/// SEPARATOR) renders as nothing in conformant clients, so a non-DM client that
/// doesn't understand the marker just shows the clean body with a trailing
/// zero-width char.
const FX_MARK: char = '\u{2063}';

fn effect_key(id: i32) -> Option<&'static str> {
    EFFECTS.iter().find(|e| e.0 == id).map(|e| e.1)
}
fn effect_emoji(id: i32) -> Option<&'static str> {
    EFFECTS.iter().find(|e| e.0 == id).map(|e| e.2)
}
fn effect_id_from_key(key: &str) -> i32 {
    EFFECTS
        .iter()
        .find(|e| e.1 == key)
        .map(|e| e.0)
        .unwrap_or(0)
}

/// Resolve an effect's emoji to its (x, y) tile in the Twemoji sheet, tolerating
/// the presence/absence of a trailing U+FE0F variation selector (the sprite
/// index and the catalog string can disagree on it).
fn effect_clip(id: i32) -> Option<(u32, u32)> {
    let emoji = effect_emoji(id)?;
    let idx = emoji_position_index();
    if let Some(p) = idx.get(emoji) {
        return Some(*p);
    }
    let stripped = emoji.trim_end_matches('\u{FE0F}');
    if let Some(p) = idx.get(stripped) {
        return Some(*p);
    }
    let with_vs = format!("{stripped}\u{FE0F}");
    idx.get(with_vs.as_str()).copied()
}

/// Append the wire marker for `effect_id` to an outgoing body. No-op for 0 or an
/// unknown id. The marker sits at the very end so stripping is a cheap suffix
/// check on receipt.
fn append_effect_marker(text: &str, effect_id: i32) -> String {
    match effect_key(effect_id) {
        Some(key) => format!("{text}{FX_MARK}dmfx:{key}{FX_MARK}"),
        None => text.to_string(),
    }
}

/// Split a trailing effect marker off a raw body, returning (clean_body,
/// effect_id). Returns the input untouched (effect 0) when there's no valid
/// marker, so it's safe to run on every body unconditionally.
fn split_effect_marker(raw: &str) -> (&str, i32) {
    let m = FX_MARK.len_utf8();
    if !raw.ends_with(FX_MARK) {
        return (raw, 0);
    }
    let head = &raw[..raw.len() - m];
    let Some(pos) = head.rfind(FX_MARK) else {
        return (raw, 0);
    };
    let inner = &head[pos + m..];
    let Some(key) = inner.strip_prefix("dmfx:") else {
        return (raw, 0);
    };
    let id = effect_id_from_key(key);
    if id == 0 {
        return (raw, 0);
    }
    (&head[..pos], id)
}

/// Set of message-ids whose effect has already been claimed for autoplay (or
/// marked seen-during-backfill). Rows rebuild from scratch (reactions, picture
/// loads, full rebuilds recreate components and re-run `init`), so the
/// fire-exactly-once decision can't live in Slint state — it lives here.
fn effect_seen_ids() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    use std::sync::OnceLock;
    static S: OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    S.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Whether this build should AUTOPLAY the effect for `message_id`. True only on
/// the very first time the id is ever built, and only if that first build is
/// `live` (a watcher arrival or optimistic send). Every later build — including
/// a full rebuild that recreates the row component and re-runs its `init`, or a
/// chat reopen — returns false, so a playing burst is never re-fired or
/// interrupted. Backfill (`live == false`) just claims the id as seen-but-quiet.
/// (Tap-to-replay is independent of this — it fires straight from the bubble.)
fn effect_should_autoplay(message_id: &str, raw_effect: i32, live: bool) -> bool {
    if raw_effect == 0 {
        return false;
    }
    let mut seen = effect_seen_ids().lock().unwrap();
    if !seen.insert(message_id.to_string()) {
        // Already present → already handled once; never autoplay again.
        return false;
    }
    live
}

// ── Markdown rendering ──────────────────────────────────────────────────────
//
// Chat bodies are parsed with `whitenoise_markdown` (the same CommonMark + GFM +
// nostr parser whitenoise-rs uses) into a `Document`, then flattened into the
// bubble's existing line/run model: each `MessageLine` is one visual line, each
// `MessageRun` an inline text/emoji cell carrying resolved styling. Block
// context (heading scale, list/blockquote indent, code plates, rules) rides on
// the line. Wrapping stays Rust-side and greedy — widths are estimated against
// the monospace advance only to decide *where* to break; Slint draws with real
// metrics, exactly as the plain-text path did before.
use whitenoise_markdown::{Block, Inline, ListItem, ListKind, NostrEntity};

/// Approximate monospace glyph advance as a fraction of font-size. Only used to
/// decide wrap points; never to position glyphs.
const MD_CHAR_W: f32 = 0.62;
/// Approximate inline-emoji advance as a fraction of font-size.
const MD_EMOJI_W: f32 = 1.25;

/// Inline styling resolved for a single run while walking the AST.
#[derive(Clone, Copy, Default)]
struct MdStyle {
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
    /// iMessage text effects as a bitmask, so any number of effects stack on
    /// the same glyph (each acts on an independent visual axis). Bits:
    /// 1 big, 2 small, 4 explode, 8 bloom, 16 shake, 32 nod, 64 ripple,
    /// 128 jitter. The RunCell decodes the bits and composes the transforms.
    fx: u8,
}

/// OR the `{name}…{/name}` effect into the style's bitmask. Effects compose, so
/// nesting (`{big}{explode}…`) keeps both. Unknown names set no bit, so the
/// span passes through as literal styled text.
fn apply_effect(style: &mut MdStyle, name: &str) {
    let bit: u8 = match name.to_ascii_lowercase().as_str() {
        "big" => 1,
        "small" => 2,
        "explode" => 4,
        "bloom" => 8,
        "shake" => 16,
        "nod" => 32,
        "ripple" => 64,
        "jitter" => 128,
        _ => 0,
    };
    style.fx |= bit;
}

impl MdStyle {
    /// True when any effect bit is set — drives per-letter splitting.
    fn has_fx(&self) -> bool {
        self.fx != 0
    }
}

/// One atomic token in the inline stream feeding the greedy wrapper.
enum MdTok {
    Word {
        text: String,
        style: MdStyle,
        link: Option<String>,
    },
    Space {
        text: String,
        style: MdStyle,
        link: Option<String>,
    },
    Emoji {
        x: u32,
        y: u32,
        fx: u8,
    },
    Break,
}

/// A wrapped line plus its block-level context, before conversion to the Slint
/// `MessageLine` struct.
struct MdLine {
    runs: Vec<MessageRun>,
    indent: f32,
    scale: f32,
    quote: i32,
    code_block: bool,
    rule: bool,
}

/// Block-walk context: accumulated left inset and current blockquote depth.
#[derive(Clone, Copy)]
struct MdCtx {
    indent: f32,
    quote: i32,
}

fn md_run_text(text: &str, style: MdStyle, link: &Option<String>) -> MessageRun {
    MessageRun {
        is_emoji: false,
        text: SharedString::from(text),
        clip_x: 0,
        clip_y: 0,
        bold: style.bold,
        italic: style.italic,
        strike: style.strike,
        code: style.code,
        link: link.as_deref().map(SharedString::from).unwrap_or_default(),
        fx: style.fx as i32,
        // Assigned in a second pass over the finished runs (md_assign_phases).
        phase: 0.0,
    }
}

fn md_run_emoji(x: u32, y: u32, fx: u8) -> MessageRun {
    MessageRun {
        is_emoji: true,
        text: SharedString::new(),
        clip_x: x as i32,
        clip_y: y as i32,
        bold: false,
        italic: false,
        strike: false,
        code: false,
        link: SharedString::new(),
        fx: fx as i32,
        phase: 0.0,
    }
}

/// Shorten a long bech32 (or any) string to `head…tail` for ergonomic display.
fn md_shorten(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= 18 {
        return s.to_string();
    }
    let head: String = chars[..12].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// Tokenize a raw text run into words / whitespace / (optionally) emoji,
/// tagging each with the active style + link. Emoji probing is skipped for
/// code spans, where glyph sequences must stay literal.
fn md_push_text(
    out: &mut Vec<MdTok>,
    text: &str,
    style: MdStyle,
    link: &Option<String>,
    positions: &std::collections::HashMap<&'static str, (u32, u32)>,
    probe_emoji: bool,
) {
    let mut buf = String::new();
    let mut buf_space = false;
    let flush = |buf: &mut String, is_space: &mut bool, out: &mut Vec<MdTok>| {
        if buf.is_empty() {
            return;
        }
        let taken = std::mem::take(buf);
        if *is_space {
            out.push(MdTok::Space {
                text: taken,
                style,
                link: link.clone(),
            });
        } else {
            out.push(MdTok::Word {
                text: taken,
                style,
                link: link.clone(),
            });
        }
        *is_space = false;
    };

    let mut i = 0;
    while i < text.len() {
        if probe_emoji {
            // Probe for the longest emoji match at `i`. ZWJ sequences can be
            // ~30+ bytes; 48 is a comfortable cap.
            let end_max = (i + 48).min(text.len());
            let mut matched: Option<(usize, u32, u32)> = None;
            for end in (i + 1..=end_max).rev() {
                if !text.is_char_boundary(end) {
                    continue;
                }
                if let Some(&(x, y)) = positions.get(&text[i..end]) {
                    matched = Some((end, x, y));
                    break;
                }
            }
            if let Some((end, x, y)) = matched {
                flush(&mut buf, &mut buf_space, out);
                out.push(MdTok::Emoji { x, y, fx: style.fx });
                i = end;
                continue;
            }
        }
        let c = text[i..].chars().next().unwrap();
        let clen = c.len_utf8();
        if c.is_whitespace() {
            if !buf_space {
                flush(&mut buf, &mut buf_space, out);
                buf_space = true;
            }
            buf.push(c);
        } else {
            if buf_space {
                flush(&mut buf, &mut buf_space, out);
            }
            buf.push(c);
            // Effect runs render per-letter so motion effects (ripple, jitter,
            // shake…) animate each glyph independently, like iMessage. Flush
            // after every visible character so each becomes its own run/cell.
            if style.has_fx() {
                flush(&mut buf, &mut buf_space, out);
            }
        }
        i += clen;
    }
    flush(&mut buf, &mut buf_space, out);
}

/// Emit a nostr entity as a single shortened, linkified word.
fn md_push_nostr(out: &mut Vec<MdTok>, e: &NostrEntity, style: MdStyle, mention: bool) {
    let mut display = md_shorten(&e.bech32);
    if mention {
        display = format!("@{display}");
    }
    out.push(MdTok::Word {
        text: display,
        style,
        link: Some(format!("nostr:{}", e.bech32)),
    });
}

/// Recursively flatten inline AST nodes into styled tokens.
fn md_walk_inlines(
    out: &mut Vec<MdTok>,
    inlines: &[Inline],
    style: MdStyle,
    link: Option<String>,
    positions: &std::collections::HashMap<&'static str, (u32, u32)>,
) {
    for inl in inlines {
        match inl {
            Inline::Text(s) => md_push_text(out, s, style, &link, positions, true),
            Inline::SoftBreak | Inline::HardBreak => out.push(MdTok::Break),
            Inline::Code(s) => {
                let st = MdStyle {
                    code: true,
                    ..style
                };
                md_push_text(out, s, st, &link, positions, false);
            }
            Inline::Emph(c) => md_walk_inlines(
                out,
                c,
                MdStyle {
                    italic: true,
                    ..style
                },
                link.clone(),
                positions,
            ),
            Inline::Strong(c) => md_walk_inlines(
                out,
                c,
                MdStyle {
                    bold: true,
                    ..style
                },
                link.clone(),
                positions,
            ),
            Inline::Strikethrough(c) => md_walk_inlines(
                out,
                c,
                MdStyle {
                    strike: true,
                    ..style
                },
                link.clone(),
                positions,
            ),
            Inline::Link { dest, children, .. } => {
                md_walk_inlines(out, children, style, Some(dest.clone()), positions)
            }
            Inline::Image { dest, alt, .. } => {
                let l = Some(dest.clone());
                md_push_text(out, "🖼", style, &l, positions, true);
                md_push_text(out, " ", style, &l, positions, false);
                if alt.is_empty() {
                    md_push_text(out, dest, style, &l, positions, false);
                } else {
                    md_walk_inlines(out, alt, style, l, positions);
                }
            }
            Inline::Autolink { url, .. } => {
                md_push_text(out, url, style, &Some(url.clone()), positions, false)
            }
            Inline::Math(s) => {
                let st = MdStyle {
                    code: true,
                    ..style
                };
                md_push_text(out, s, st, &link, positions, false);
            }
            Inline::NostrMention(e) => md_push_nostr(out, e, style, true),
            Inline::NostrUri(e) => md_push_nostr(out, e, style, false),
            Inline::Effect { name, children } => {
                // Set the matching channel (size or motion) on top of the
                // inherited style, so nesting stacks instead of overwriting.
                // Unknown names leave both channels untouched → pass-through.
                let mut st = style;
                apply_effect(&mut st, name);
                md_walk_inlines(out, children, st, link.clone(), positions);
            }
        }
    }
}

/// Font scale for an ATX heading level.
fn md_heading_scale(level: u8) -> f32 {
    match level {
        1 => 1.5,
        2 => 1.34,
        3 => 1.2,
        4 => 1.1,
        _ => 1.04,
    }
}

/// A thin blank line used to separate sibling blocks.
fn md_spacer(ctx: MdCtx) -> MdLine {
    MdLine {
        runs: vec![md_run_text(" ", MdStyle::default(), &None)],
        indent: ctx.indent,
        scale: 0.4,
        quote: ctx.quote,
        code_block: false,
        rule: false,
    }
}

/// Greedy-pack a token stream into wrapped lines under `max_width` (minus the
/// block indent). Over-long single tokens (URLs, code) are hard-split so they
/// never overflow the bubble.
#[allow(clippy::too_many_arguments)]
fn md_wrap(
    out: &mut Vec<MdLine>,
    toks: Vec<MdTok>,
    max_width: f32,
    base_fs: f32,
    indent: f32,
    scale: f32,
    quote: i32,
    code_block: bool,
) {
    let char_w = base_fs * MD_CHAR_W * scale;
    let emoji_w = base_fs * MD_EMOJI_W * scale;
    let avail = (max_width - indent).max(40.0);
    let max_chars = ((avail / char_w).floor() as usize).max(1);

    let mut cur: Vec<MessageRun> = Vec::new();
    let mut x = 0.0f32;
    let flush = |out: &mut Vec<MdLine>, cur: &mut Vec<MessageRun>| {
        out.push(MdLine {
            runs: std::mem::take(cur),
            indent,
            scale,
            quote,
            code_block,
            rule: false,
        });
    };

    for tok in toks {
        match tok {
            MdTok::Break => {
                flush(out, &mut cur);
                x = 0.0;
            }
            MdTok::Space { text, style, link } => {
                // Drop whitespace at the start of a wrapped line — except in
                // code blocks, where leading indentation is significant.
                if x == 0.0 && !code_block {
                    continue;
                }
                x += text.chars().count() as f32 * char_w;
                cur.push(md_run_text(&text, style, &link));
            }
            MdTok::Emoji { x: ex, y: ey, fx } => {
                if x > 0.0 && x + emoji_w > avail {
                    flush(out, &mut cur);
                    x = 0.0;
                }
                cur.push(md_run_emoji(ex, ey, fx));
                x += emoji_w;
            }
            MdTok::Word { text, style, link } => {
                let n = text.chars().count();
                let w = n as f32 * char_w;
                if w <= avail {
                    if x > 0.0 && x + w > avail {
                        flush(out, &mut cur);
                        x = 0.0;
                    }
                    cur.push(md_run_text(&text, style, &link));
                    x += w;
                } else {
                    // Hard-split an over-long token into width-fitting chunks.
                    let chars: Vec<char> = text.chars().collect();
                    let mut start = 0;
                    while start < chars.len() {
                        if x > 0.0 {
                            flush(out, &mut cur);
                            x = 0.0;
                        }
                        let end = (start + max_chars).min(chars.len());
                        let chunk: String = chars[start..end].iter().collect();
                        cur.push(md_run_text(&chunk, style, &link));
                        x += (end - start) as f32 * char_w;
                        start = end;
                    }
                }
            }
        }
    }
    if !cur.is_empty() {
        flush(out, &mut cur);
    }
}

/// Render one table row as a wrapped line, cells separated by a thin divider.
#[allow(clippy::too_many_arguments)]
fn md_emit_table_row(
    out: &mut Vec<MdLine>,
    cells: &[whitenoise_markdown::TableCell],
    header: bool,
    ctx: MdCtx,
    max_width: f32,
    base_fs: f32,
    positions: &std::collections::HashMap<&'static str, (u32, u32)>,
) {
    let mut toks = Vec::new();
    for (ci, cell) in cells.iter().enumerate() {
        if ci > 0 {
            toks.push(MdTok::Word {
                text: "│".to_string(),
                style: MdStyle::default(),
                link: None,
            });
            toks.push(MdTok::Space {
                text: " ".to_string(),
                style: MdStyle::default(),
                link: None,
            });
        }
        md_walk_inlines(
            &mut toks,
            &cell.inlines,
            MdStyle {
                bold: header,
                ..Default::default()
            },
            None,
            positions,
        );
        toks.push(MdTok::Space {
            text: " ".to_string(),
            style: MdStyle::default(),
            link: None,
        });
    }
    md_wrap(
        out, toks, max_width, base_fs, ctx.indent, 1.0, ctx.quote, false,
    );
}

/// Render the items of a list, placing each item's marker on its first line and
/// indenting wrapped / nested content under it.
#[allow(clippy::too_many_arguments)]
fn md_walk_list(
    out: &mut Vec<MdLine>,
    kind: ListKind,
    tight: bool,
    items: &[ListItem],
    ctx: MdCtx,
    max_width: f32,
    base_fs: f32,
    positions: &std::collections::HashMap<&'static str, (u32, u32)>,
) {
    let mut number = match kind {
        ListKind::Ordered { start, .. } => start,
        ListKind::Bullet { .. } => 0,
    };
    for (ii, item) in items.iter().enumerate() {
        if ii > 0 && !tight {
            out.push(md_spacer(ctx));
        }
        let mut marker = match kind {
            ListKind::Ordered { .. } => {
                let m = format!("{number}. ");
                number += 1;
                m
            }
            ListKind::Bullet { .. } => "•  ".to_string(),
        };
        if let Some(checked) = item.checked {
            marker.push_str(if checked { "☑ " } else { "☐ " });
        }
        let marker_w = marker.chars().count() as f32 * base_fs * MD_CHAR_W;
        let child = MdCtx {
            indent: ctx.indent + marker_w,
            quote: ctx.quote,
        };
        let mut tmp: Vec<MdLine> = Vec::new();
        md_walk_blocks(&mut tmp, &item.blocks, child, max_width, base_fs, positions);
        if tmp.is_empty() {
            tmp.push(MdLine {
                runs: Vec::new(),
                indent: child.indent,
                scale: 1.0,
                quote: ctx.quote,
                code_block: false,
                rule: false,
            });
        }
        // The marker sits at the item's own indent; content trails after it.
        tmp[0].indent = ctx.indent;
        tmp[0]
            .runs
            .insert(0, md_run_text(&marker, MdStyle::default(), &None));
        out.extend(tmp);
    }
}

/// Recursively flatten block AST nodes into wrapped, context-tagged lines.
fn md_walk_blocks(
    out: &mut Vec<MdLine>,
    blocks: &[Block],
    ctx: MdCtx,
    max_width: f32,
    base_fs: f32,
    positions: &std::collections::HashMap<&'static str, (u32, u32)>,
) {
    for (bi, b) in blocks.iter().enumerate() {
        if bi > 0 {
            out.push(md_spacer(ctx));
        }
        match b {
            Block::Paragraph { inlines } => {
                let mut toks = Vec::new();
                md_walk_inlines(&mut toks, inlines, MdStyle::default(), None, positions);
                md_wrap(
                    out, toks, max_width, base_fs, ctx.indent, 1.0, ctx.quote, false,
                );
            }
            Block::Heading { level, inlines } => {
                let mut toks = Vec::new();
                md_walk_inlines(
                    &mut toks,
                    inlines,
                    MdStyle {
                        bold: true,
                        ..Default::default()
                    },
                    None,
                    positions,
                );
                md_wrap(
                    out,
                    toks,
                    max_width,
                    base_fs,
                    ctx.indent,
                    md_heading_scale(*level),
                    ctx.quote,
                    false,
                );
            }
            Block::ThematicBreak => out.push(MdLine {
                runs: Vec::new(),
                indent: ctx.indent,
                scale: 1.0,
                quote: ctx.quote,
                code_block: false,
                rule: true,
            }),
            Block::CodeBlock { content, .. } => {
                let body = content.strip_suffix('\n').unwrap_or(content);
                let st = MdStyle {
                    code: true,
                    ..Default::default()
                };
                for line in body.split('\n') {
                    if line.is_empty() {
                        out.push(MdLine {
                            runs: vec![md_run_text(" ", st, &None)],
                            indent: ctx.indent,
                            scale: 1.0,
                            quote: ctx.quote,
                            code_block: true,
                            rule: false,
                        });
                        continue;
                    }
                    let mut toks = Vec::new();
                    md_push_text(&mut toks, line, st, &None, positions, false);
                    md_wrap(
                        out, toks, max_width, base_fs, ctx.indent, 1.0, ctx.quote, true,
                    );
                }
            }
            Block::BlockQuote { blocks } => {
                let inner = MdCtx {
                    indent: ctx.indent + 12.0,
                    quote: ctx.quote + 1,
                };
                md_walk_blocks(out, blocks, inner, max_width, base_fs, positions);
            }
            Block::List { kind, tight, items } => {
                md_walk_list(
                    out, *kind, *tight, items, ctx, max_width, base_fs, positions,
                );
            }
            Block::Table { header, rows, .. } => {
                md_emit_table_row(out, header, true, ctx, max_width, base_fs, positions);
                for row in rows {
                    md_emit_table_row(out, row, false, ctx, max_width, base_fs, positions);
                }
            }
            Block::MathBlock { content } => {
                let body = content.strip_suffix('\n').unwrap_or(content);
                let st = MdStyle {
                    code: true,
                    ..Default::default()
                };
                for line in body.split('\n') {
                    let mut toks = Vec::new();
                    md_push_text(&mut toks, line, st, &None, positions, false);
                    md_wrap(
                        out, toks, max_width, base_fs, ctx.indent, 1.0, ctx.quote, true,
                    );
                }
            }
        }
    }
}

/// Parse a chat-message body as Markdown and flatten it into pre-wrapped lines.
/// Second pass over finished runs: stagger each effect run's `phase` so motion
/// effects animate per-letter rather than in lockstep. The counter advances
/// once per effect cell (giving Ripple its travelling crest and Jitter its
/// decorrelated wobble) and resets on any non-effect run so each contiguous
/// span starts its wave fresh.
fn md_assign_phases(lines: &mut [MdLine]) {
    let mut step: u32 = 0;
    for line in lines.iter_mut() {
        for run in line.runs.iter_mut() {
            if run.fx != 0 {
                run.phase = step as f32 * 0.12;
                step += 1;
            } else {
                step = 0;
            }
        }
    }
}

fn tokenize_message_lines(text: &str, max_width: f32, base_fs: f32) -> Vec<MessageLine> {
    let positions = emoji_position_index();
    let doc = whitenoise_markdown::parse(text);
    let mut lines: Vec<MdLine> = Vec::new();
    md_walk_blocks(
        &mut lines,
        &doc.blocks,
        MdCtx {
            indent: 0.0,
            quote: 0,
        },
        max_width,
        base_fs,
        positions,
    );
    md_assign_phases(&mut lines);
    lines
        .into_iter()
        .map(|l| MessageLine {
            runs: ModelRc::new(VecModel::from(l.runs)),
            indent: l.indent,
            scale: l.scale,
            quote: l.quote,
            code_block: l.code_block,
            rule: l.rule,
        })
        .collect()
}

/// Open a URL (or `mailto:` / `nostr:` URI) with the platform's default
/// handler. Fire-and-forget; failures are logged, not surfaced.
fn open_external(url: &str) {
    if url.is_empty() {
        return;
    }
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";
    match std::process::Command::new(program).arg(url).spawn() {
        Ok(_) => tracing::debug!(url, "opened external link"),
        Err(e) => tracing::warn!(url, error = %e, "failed to open external link"),
    }
}

/// Find an active `@mention` token ending at the caret. Returns the byte offset
/// of the `@` and the query text between it and the caret. The token is active
/// only when the `@` sits at the start or directly after whitespace, and there
/// is no whitespace between it and the caret.
fn detect_mention(text: &str, cursor: usize) -> Option<(usize, String)> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let prefix = &text[..cursor];
    let mut at_byte = None;
    for (i, c) in prefix.char_indices().rev() {
        if c == '@' {
            let preceded_ok = i == 0
                || prefix[..i]
                    .chars()
                    .next_back()
                    .map(|p| p.is_whitespace())
                    .unwrap_or(true);
            if preceded_ok {
                at_byte = Some(i);
            }
            break;
        }
        if c.is_whitespace() {
            break;
        }
    }
    let at = at_byte?;
    Some((at, prefix[at + 1..].to_string()))
}

/// Filter the open chat's members by a mention query (matches display name,
/// short npub, or full npub; an empty query lists everyone). Capped at 50.
fn filter_mention_candidates(ui: &DarkMatterLinux, query: &str) -> Vec<GroupMember> {
    let q = query.to_lowercase();
    ui.get_chat_members()
        .iter()
        .filter(|m| {
            if q.is_empty() {
                return true;
            }
            if m.name.as_str().to_lowercase().contains(&q)
                || m.npub_short.as_str().to_lowercase().contains(&q)
            {
                return true;
            }
            npub_for_account_id(m.member_id.as_str())
                .map(|n| n.to_lowercase().contains(&q))
                .unwrap_or(false)
        })
        .take(50)
        .collect()
}

/// Splice the chosen member's npub over the active `@token` and place the caret
/// just after the inserted mention.
fn commit_mention(
    ui: &DarkMatterLinux,
    mention_span: &Rc<RefCell<Option<(usize, usize)>>>,
    index: i32,
) {
    ui.set_mention_active(false);
    let Some((at, end)) = mention_span.borrow_mut().take() else {
        return;
    };
    if index < 0 {
        return;
    }
    let cands = ui.get_mention_candidates();
    let Some(member) = cands.row_data(index as usize) else {
        return;
    };
    let npub = npub_for_account_id(member.member_id.as_str())
        .unwrap_or_else(|_| member.member_id.to_string());
    let mut draft = ui.get_composer_draft().to_string();
    if at > end || end > draft.len() || !draft.is_char_boundary(at) || !draft.is_char_boundary(end)
    {
        return;
    }
    let insert = format!("@{npub} ");
    let new_cursor = at + insert.len();
    draft.replace_range(at..end, &insert);
    ui.set_composer_draft(draft.into());
    // Move the caret past the inserted mention (tick forces re-apply).
    ui.set_composer_caret_pos(new_cursor as i32);
    ui.set_composer_caret_tick(ui.get_composer_caret_tick().wrapping_add(1));
}

// Memoized markdown line models, keyed by (body, wrap-width). Rebuilding a
// chat re-parses every visible body through the full markdown → wrap pipeline;
// bodies are immutable (edits arrive as new text → new key), so the flattened
// model can be shared across rows and rebuilds. UI-thread only (the line
// models hold `ModelRc`s, which are not `Send`). Bounded: wholesale-cleared
// at the cap rather than LRU-tracked — a full re-parse of one chat is cheap
// compared to bookkeeping on every hit.
thread_local! {
    static MESSAGE_LINES_CACHE: RefCell<HashMap<(String, u32), ModelRc<MessageLine>>> =
        RefCell::new(HashMap::new());
}
const MESSAGE_LINES_CACHE_CAP: usize = 4096;

/// Build the `lines` model for `ChatMessage` from the message body.
fn build_message_lines(text: &str, bubble_max: f32) -> ModelRc<MessageLine> {
    // Chat-body chrome: 2*pad-h (14) + gap (12) + meta col (~70). Conservative
    // so wrapping kicks in before the dynamic `available-w` clips the bubble.
    let budget = (bubble_max - 110.0).max(60.0);
    MESSAGE_LINES_CACHE.with(|cache| {
        let key = (text.to_string(), budget.to_bits());
        if let Some(model) = cache.borrow().get(&key) {
            return model.clone();
        }
        let model: ModelRc<MessageLine> =
            ModelRc::new(VecModel::from(tokenize_message_lines(text, budget, 13.0)));
        let mut cache = cache.borrow_mut();
        if cache.len() >= MESSAGE_LINES_CACHE_CAP {
            cache.clear();
        }
        cache.insert(key, model.clone());
        model
    })
}

/// Telegram-style jumbo-emoji test. If `text` is nothing but emoji (plus
/// whitespace) and short enough — at most [`JUMBO_EMOJI_MAX`] glyphs — return
/// the emoji count; otherwise 0. The probe mirrors the tokenizer's longest-
/// match-against-the-sprite-table logic ([`md_push_text`]) so what we classify
/// as jumbo is exactly what would render as sprite cells.
const JUMBO_EMOJI_MAX: u32 = 3;
fn jumbo_emoji_count(text: &str) -> u32 {
    let positions = emoji_position_index();
    let t = text.trim();
    if t.is_empty() {
        return 0;
    }
    let mut count = 0u32;
    let mut i = 0usize;
    while i < t.len() {
        let c = t[i..].chars().next().unwrap();
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        // Longest emoji match at `i` (ZWJ sequences run ~30+ bytes; 48 caps it).
        let end_max = (i + 48).min(t.len());
        let mut matched = None;
        for end in (i + 1..=end_max).rev() {
            if t.is_char_boundary(end) && positions.contains_key(&t[i..end]) {
                matched = Some(end);
                break;
            }
        }
        match matched {
            Some(end) => {
                count += 1;
                if count > JUMBO_EMOJI_MAX {
                    return 0;
                }
                i = end;
            }
            // Any non-emoji glyph disqualifies the whole message.
            None => return 0,
        }
    }
    count
}

/// Filter the full emoji catalog by `query` (matches name + shortcodes,
/// lowercased substring) and return the flat list. The Slint side handles
/// column packing and virtualization.
fn build_emoji_list(query: &str) -> Vec<EmojiEntry> {
    let q = query.trim().to_lowercase();
    let positions = emoji_position_index();
    let mut hits: Vec<EmojiEntry> = Vec::new();
    for e in emojis::iter() {
        if !q.is_empty() {
            let name_match = e.name().to_lowercase().contains(&q);
            let code_match = e.shortcodes().any(|c| c.to_lowercase().contains(&q));
            if !name_match && !code_match {
                continue;
            }
        }
        // Skip emojis the build-time sprite sheet doesn't cover.
        let Some(&(x, y)) = positions.get(e.as_str()) else {
            continue;
        };
        hits.push(EmojiEntry {
            emoji: s(e.as_str()),
            name: s(e.name()),
            clip_x: x as i32,
            clip_y: y as i32,
        });
    }
    hits
}

/// Run [`copy_to_clipboard`] on a throwaway worker thread and hand the result
/// back on the UI thread. The CLI helpers and arboard can all wait on the
/// display server (wedged compositor, full pipe buffer, slow X11 connect), so
/// the UI thread must never call [`copy_to_clipboard`] directly.
fn copy_to_clipboard_async(
    text: String,
    on_done: impl FnOnce(Result<(), String>) + Send + 'static,
) {
    std::thread::spawn(move || {
        let result = copy_to_clipboard(&text);
        let _ = slint::invoke_from_event_loop(move || on_done(result));
    });
}

/// Push `text` to the system clipboard. Blocking — see
/// [`copy_to_clipboard_async`] for the only form UI callbacks may use.
///
/// On Linux/FreeBSD, arboard's Wayland support is finicky — it depends on
/// the compositor exposing the right data-control protocol, and on some
/// stacks `set_text` returns `Ok` without anyone actually getting the
/// content. Instead, we prefer the standard CLI tools that ship in every
/// desktop install: `wl-copy` for Wayland, `xclip`/`xsel` for X11. On macOS
/// the native `pbcopy` helper is always present (and the Wayland/X11 tools
/// would talk to an XQuartz clipboard, not the system one, so they are
/// skipped entirely). arboard stays as a final fallback everywhere.
fn copy_to_clipboard(text: &str) -> Result<(), String> {
    let preview: String = text.chars().take(24).collect();
    eprintln!(
        "[clipboard] copy len={} preview={:?}{} WAYLAND_DISPLAY={:?} DISPLAY={:?}",
        text.len(),
        preview,
        if text.len() > 24 { "…" } else { "" },
        std::env::var_os("WAYLAND_DISPLAY"),
        std::env::var_os("DISPLAY"),
    );

    #[cfg(target_os = "macos")]
    {
        match copy_via_command("pbcopy", &[], text) {
            Ok(()) => {
                eprintln!("[clipboard] via pbcopy ok");
                return Ok(());
            }
            Err(e) => eprintln!("[clipboard] pbcopy failed: {e}"),
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        // Wayland session?
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            match copy_via_command("wl-copy", &[], text) {
                Ok(()) => {
                    eprintln!("[clipboard] via wl-copy ok");
                    return Ok(());
                }
                Err(e) => eprintln!("[clipboard] wl-copy failed: {e}"),
            }
        }

        // X11 session?
        if std::env::var_os("DISPLAY").is_some() {
            for (cmd, args) in [
                ("xclip", &["-selection", "clipboard"][..]),
                ("xsel", &["--clipboard", "--input"][..]),
            ] {
                match copy_via_command(cmd, args, text) {
                    Ok(()) => {
                        eprintln!("[clipboard] via {cmd} ok");
                        return Ok(());
                    }
                    Err(e) => eprintln!("[clipboard] {cmd} failed: {e}"),
                }
            }
        }
    }

    // Last resort: arboard. Hold a single long-lived Clipboard so we don't
    // immediately drop ownership.
    use std::sync::{Mutex, OnceLock};
    static CLIPBOARD: OnceLock<Mutex<arboard::Clipboard>> = OnceLock::new();
    let cb = CLIPBOARD.get_or_init(|| {
        Mutex::new(arboard::Clipboard::new().expect("clipboard backend init failed"))
    });
    let mut guard = cb.lock().map_err(|e| e.to_string())?;
    match guard.set_text(text.to_string()) {
        Ok(()) => {
            eprintln!("[clipboard] via arboard ok");
            Ok(())
        }
        Err(e) => {
            eprintln!("[clipboard] arboard failed: {e}");
            Err(e.to_string())
        }
    }
}

/// Spawn a CLI clipboard helper, pipe `text` into its stdin, wait for the
/// parent to exit (these helpers fork themselves into the background after
/// reading stdin, so the parent exits in milliseconds), and surface the
/// exit code if anything went wrong.
///
/// stdout/stderr must NOT be `Stdio::piped()`: the forked background child
/// that keeps serving the clipboard inherits the pipe write ends, so reading
/// them to EOF (e.g. `wait_with_output`) blocks until clipboard ownership is
/// lost — which freezes the UI thread. stderr is inherited instead, so any
/// helper diagnostics still land in our own stderr log.
fn copy_via_command(cmd: &str, args: &[&str], text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    eprintln!("[clipboard] spawning: {cmd} {args:?}");
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| format!("write stdin: {e}"))?;
        // dropping stdin closes the pipe so the helper sees EOF
    }
    let status = child.wait().map_err(|e| format!("wait: {e}"))?;
    if !status.success() {
        return Err(format!("{cmd} exited {status} (stderr passed through)"));
    }
    Ok(())
}

/// Choose an image MIME target from a clipboard target list, applying the
/// image-intent rule: any plain-text target wins (returns `None` — the
/// native text paste already handled it, and sources that offer *both*
/// mean text), otherwise `image/png` is preferred over whatever other
/// `image/*` target comes first.
fn pick_image_target(types: &[&str]) -> Option<String> {
    let has_text = types
        .iter()
        .any(|t| *t == "UTF8_STRING" || *t == "STRING" || t.starts_with("text/plain"));
    if has_text {
        return None;
    }
    if types.contains(&"image/png") {
        return Some("image/png".to_string());
    }
    types
        .iter()
        .find(|t| t.starts_with("image/"))
        .map(|t| t.to_string())
}

/// Read image bytes off the system clipboard, but only when the clipboard
/// looks image-intent (see [`pick_image_target`]). Mirrors
/// [`copy_to_clipboard`]'s platform ladder: `wl-paste` on Wayland,
/// `xclip` on X11, arboard as the fallback everywhere and the primary
/// path on macOS (`pbpaste` is text-only). Blocking — subprocess /
/// display-server round-trips — so never call on the UI thread. Returns
/// `(bytes, media_type)`.
fn paste_image_from_clipboard() -> Option<(Vec<u8>, String)> {
    #[cfg(not(target_os = "macos"))]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            match paste_via_command("wl-paste", &["--list-types"]) {
                Ok(out) => {
                    let listing = String::from_utf8_lossy(&out).into_owned();
                    let types: Vec<&str> = listing.lines().map(str::trim).collect();
                    // wl-paste answered: it owns the truth about this
                    // clipboard, so no image target (or text intent) is a
                    // final no — don't fall through to arboard.
                    let mime = pick_image_target(&types)?;
                    return match paste_via_command("wl-paste", &["--no-newline", "--type", &mime]) {
                        Ok(bytes) if !bytes.is_empty() => {
                            eprintln!(
                                "[clipboard] image via wl-paste ({mime}, {} bytes)",
                                bytes.len()
                            );
                            Some((bytes, mime))
                        }
                        Ok(_) => None,
                        Err(e) => {
                            eprintln!("[clipboard] wl-paste read failed: {e}");
                            None
                        }
                    };
                }
                Err(e) => eprintln!("[clipboard] wl-paste list-types failed: {e}"),
            }
        }
        if std::env::var_os("DISPLAY").is_some() {
            match paste_via_command("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"]) {
                Ok(out) => {
                    let listing = String::from_utf8_lossy(&out).into_owned();
                    let types: Vec<&str> = listing.lines().map(str::trim).collect();
                    let mime = pick_image_target(&types)?;
                    return match paste_via_command(
                        "xclip",
                        &["-selection", "clipboard", "-t", &mime, "-o"],
                    ) {
                        Ok(bytes) if !bytes.is_empty() => {
                            eprintln!(
                                "[clipboard] image via xclip ({mime}, {} bytes)",
                                bytes.len()
                            );
                            Some((bytes, mime))
                        }
                        Ok(_) => None,
                        Err(e) => {
                            eprintln!("[clipboard] xclip read failed: {e}");
                            None
                        }
                    };
                }
                Err(e) => eprintln!("[clipboard] xclip targets failed: {e}"),
            }
        }
    }

    // arboard fallback. It hands back raw RGBA, so re-encode as PNG for the
    // upload path (which wants original compressed bytes).
    let mut cb = match arboard::Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[clipboard] arboard init failed: {e}");
            return None;
        }
    };
    if matches!(cb.get_text(), Ok(t) if !t.is_empty()) {
        return None; // text intent — native paste already handled it
    }
    let img = cb.get_image().ok()?;
    let (w, h) = (img.width as u32, img.height as u32);
    let rgba = image::RgbaImage::from_raw(w, h, img.bytes.into_owned())?;
    let mut png = Vec::new();
    if let Err(e) = image::DynamicImage::ImageRgba8(rgba)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
    {
        eprintln!("[clipboard] png encode failed: {e}");
        return None;
    }
    eprintln!("[clipboard] image via arboard ({w}x{h})");
    Some((png, "image/png".to_string()))
}

/// Run a CLI clipboard *reader* and capture its stdout bytes. Unlike the
/// copy helpers these don't fork into the background, so a plain
/// `output()` is safe.
fn paste_via_command(cmd: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    use std::process::{Command, Stdio};
    let out = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!("{cmd} exited {}", out.status));
    }
    Ok(out.stdout)
}

// ─── Keys & key packages ───────────────────────────────────────────────

fn kp_to_ui(rec: &marmot_app::AccountKeyPackageRecord) -> KeyPackageInfo {
    let short_ref: String = rec.key_package_ref_hex.chars().take(16).collect();
    let short_ref = if rec.key_package_ref_hex.len() > 16 {
        format!("{short_ref}…")
    } else {
        short_ref
    };
    KeyPackageInfo {
        key_package_id: s(&rec.key_package_id),
        key_package_ref: s(&short_ref),
        event_id: s(&rec.key_package_event_id),
        published_at: s(&format_date_unix(rec.published_at)),
        relay_count: rec.source_relays.len() as i32,
        local: rec.local,
        on_relay: rec.relay,
    }
}

/// Populate the Keys page from local-only KP state (no relay round-trip).
/// Used at boot and after publish/rotate so the UI reflects what's on disk
/// immediately, while a relay refresh runs in the background.
/// Read the local key packages (on-disk JSON) + relay list on the backend
/// runtime, then push the rows on the UI thread.
fn refresh_kp_local_async(ui: &DarkMatterLinux, backend: &Arc<Backend>) {
    let weak = ui.as_weak();
    let b = backend.clone();
    backend.tokio_handle().spawn(async move {
        let local = b.key_packages_local();
        let relays = b.key_package_relays();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            // "Published" means actually out on the network — a KP with a
            // published event id, or one we've observed on a relay. A purely
            // local KP (which always exists once the account boots) does NOT
            // count.
            let published = local
                .iter()
                .any(|kp| !kp.key_package_event_id.is_empty() || kp.relay);
            ui.set_kp_published(published);
            let rows: Vec<KeyPackageInfo> = local.iter().map(kp_to_ui).collect();
            ui.set_key_packages(ModelRc::new(VecModel::from(rows)));
            let relays: Vec<SharedString> = relays.into_iter().map(SharedString::from).collect();
            ui.set_kp_relays(ModelRc::new(VecModel::from(relays)));
        });
    });
}

/// Push the on-disk relay list into the UI model. Used after add/remove.
fn push_network_relays(ui: &DarkMatterLinux, list: &[String]) {
    let rows: Vec<SharedString> = list.iter().cloned().map(SharedString::from).collect();
    ui.set_network_relays(ModelRc::new(VecModel::from(rows)));
    // Keep the one-click suggestions in sync: only offer ones not already added.
    push_suggested_relays(ui, list);
}

/// Well-known public relays offered as one-click adds on the get-started screen.
/// DEV POLICY: whitenoise official relays only while in development — these are
/// where the mobile apps publish, so dev peers are always mutually discoverable.
/// Before release, broaden again (e.g. relay.primal.net, relay.ditto.pub).
const SUGGESTED_RELAYS: &[&str] = &[
    "wss://relay.eu.whitenoise.chat",
    "wss://relay.us.whitenoise.chat",
];

/// Publish the suggested-relay chips = `SUGGESTED_RELAYS` minus whatever the user
/// already has, so a suggestion vanishes once it's added.
fn push_suggested_relays(ui: &DarkMatterLinux, current: &[String]) {
    let suggestions: Vec<SharedString> = SUGGESTED_RELAYS
        .iter()
        .filter(|s| !current.iter().any(|u| u.eq_ignore_ascii_case(s)))
        .map(|s| SharedString::from(*s))
        .collect();
    ui.set_suggested_relays(ModelRc::new(VecModel::from(suggestions)));
}

/// Collect a `[string]` Slint model into an owned `Vec<String>`.
fn vec_string_from_model(model: &ModelRc<SharedString>) -> Vec<String> {
    model.iter().map(|s| s.to_string()).collect()
}

/// Validate a user-entered relay URL. Trim is the caller's job.
fn validate_relay_url(url: &str) -> Result<(), String> {
    if url.is_empty() {
        return Err("Enter a relay URL.".to_string());
    }
    if !(url.starts_with("wss://") || url.starts_with("ws://")) {
        return Err("Must start with wss:// or ws://".to_string());
    }
    if url.len() < 8 {
        return Err("Relay URL looks too short.".to_string());
    }
    Ok(())
}

/// Push the booted-relays list + current health into the UI. Called after
/// the backend finishes booting.
fn refresh_network_post_boot(backend: &Arc<Backend>, ui: &DarkMatterLinux) {
    let booted: Vec<SharedString> = backend
        .booted_relays()
        .iter()
        .cloned()
        .map(SharedString::from)
        .collect();
    ui.set_network_booted_relays(ModelRc::new(VecModel::from(booted)));
    // `relay_health` does a `block_on` into the relay plane — poll it from a
    // worker so this post-boot UI pass never stalls the event loop.
    let weak = ui.as_weak();
    let backend = backend.clone();
    std::thread::spawn(move || {
        let (connected, total) = backend.relay_health();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_network_connected(connected as i32);
            ui.set_network_total(total as i32);
            // Mark the first sync so the chat-list footer leaves "SYNCING…"
            // and the 1s timer starts counting up from a real baseline.
            ui.set_sync_secs(0);
        });
    });
}

// `Mon DD · HH:MM` (local time) for KP timestamps. Returns "" for zero (unknown).
// ─── User-selectable stamp formats ──────────────────────────────────────
// Mirrors `Settings::{time_format,date_format}` as process-wide atomics so
// the formatters (called per row in rebuild loops) never touch disk. Synced
// at boot and whenever the user changes the pickers in Settings → General.

static TIME_FORMAT_12H: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// 0 = mdy ("Jun 12"), 1 = dmy ("12 Jun"), 2 = iso ("2026-06-12").
static DATE_FORMAT_KIND: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn apply_stamp_formats(settings: &Settings) {
    use std::sync::atomic::Ordering;
    TIME_FORMAT_12H.store(settings.time_format == "12h", Ordering::Relaxed);
    let kind = match settings.date_format.as_str() {
        "dmy" => 1,
        "iso" => 2,
        _ => 0,
    };
    DATE_FORMAT_KIND.store(kind, Ordering::Relaxed);
}

/// Clock part of a stamp, honoring the 12h/24h preference.
fn format_clock(z: &jiff::Zoned) -> String {
    if TIME_FORMAT_12H.load(std::sync::atomic::Ordering::Relaxed) {
        let (h, half) = match z.hour() {
            0 => (12, "AM"),
            h @ 1..=11 => (h, "AM"),
            12 => (12, "PM"),
            h => (h - 12, "PM"),
        };
        format!("{}:{:02} {}", h, z.minute(), half)
    } else {
        format!("{:02}:{:02}", z.hour(), z.minute())
    }
}

/// Date part of a stamp, honoring the mdy/dmy/iso preference. `with_year`
/// is advisory for the named-month styles; ISO always carries the year.
fn format_date_part(z: &jiff::Zoned, with_year: bool) -> String {
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mi = (z.month() as usize).saturating_sub(1).min(11);
    match DATE_FORMAT_KIND.load(std::sync::atomic::Ordering::Relaxed) {
        1 => {
            if with_year {
                format!("{} {} {}", z.day(), months[mi], z.year())
            } else {
                format!("{} {}", z.day(), months[mi])
            }
        }
        2 => format!("{:04}-{:02}-{:02}", z.year(), z.month(), z.day()),
        _ => {
            if with_year {
                format!("{} {} {}", months[mi], z.day(), z.year())
            } else {
                format!("{} {}", months[mi], z.day())
            }
        }
    }
}

fn format_date_unix(secs: u64) -> String {
    if secs == 0 {
        return String::new();
    }
    let z = local_time(secs);
    format!("{} · {}", format_date_part(&z, false), format_clock(&z))
}

/// Render a unix-seconds timestamp as a clock stamp in the user's local
/// timezone, honoring the 12h/24h preference.
fn format_unix(secs: u64) -> String {
    let z = local_time(secs);
    format_clock(&z)
}

/// Friendly chat-list stamp: `HH:MM` for today, "Yesterday", the weekday
/// within the last week, `Mon DD` within the year, `Mon DD YYYY` beyond.
/// Date-granular on purpose — labels only go stale at midnight, so the
/// refresh is a once-a-day model rebuild instead of a per-minute tick.
/// (English like the month names in `format_date_unix`; gettext only covers
/// .slint strings today.)
fn format_chat_stamp(secs: u64) -> String {
    if secs == 0 {
        return String::new();
    }
    let z = local_time(secs);
    let now = jiff::Zoned::now();
    let days = z
        .date()
        .until(now.date())
        .map(|span| span.get_days())
        .unwrap_or(0);
    if days <= 0 {
        // Today — or a clock-skewed future stamp, which gets the same benefit
        // of the doubt rather than a nonsense date.
        return format_clock(&z);
    }
    if days == 1 {
        return "Yesterday".to_string();
    }
    if days < 7 {
        use jiff::civil::Weekday;
        return match z.weekday() {
            Weekday::Monday => "Mon",
            Weekday::Tuesday => "Tue",
            Weekday::Wednesday => "Wed",
            Weekday::Thursday => "Thu",
            Weekday::Friday => "Fri",
            Weekday::Saturday => "Sat",
            Weekday::Sunday => "Sun",
        }
        .to_string();
    }
    format_date_part(&z, z.year() != now.year())
}

/// Epoch seconds → civil time in the system timezone. Conversion happens
/// per-timestamp (not via a cached offset) so messages on either side of a
/// DST switch each get the offset that was in effect when they were sent.
fn local_time(secs: u64) -> jiff::Zoned {
    jiff::Timestamp::from_second(secs.min(i64::MAX as u64) as i64)
        .unwrap_or_default()
        .to_zoned(jiff::tz::TimeZone::system())
}

// ─── Contacts ──────────────────────────────────────────────────────────

/// Fetch the follow list AND the nickname map (a disk read — `Settings`
/// lives in a JSON file) on the backend runtime, then build + apply the
/// contact rows (+ avatar fetches) on the UI thread. `then` runs last, still
/// on the UI thread, for follow-ups that need the refreshed model (e.g.
/// selecting a freshly-added contact).
fn refresh_contacts_async(
    ui: &DarkMatterLinux,
    backend: &Arc<Backend>,
    then: impl FnOnce(&DarkMatterLinux) + Send + 'static,
) {
    let weak = ui.as_weak();
    let b = backend.clone();
    backend.tokio_handle().spawn(async move {
        let records = match b.follow_list() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[backend] follow_list failed: {e:#}");
                return;
            }
        };
        let nicknames = Settings::load().nicknames;
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            let contacts = ui.get_contacts();
            let rows: Vec<Contact> = records
                .iter()
                .map(|r| contact_from(r, &nicknames))
                .collect();
            if let Some(vm) = contacts.as_any().downcast_ref::<VecModel<Contact>>() {
                vm.set_vec(rows);
            }
            spawn_contact_avatar_fetches(&ui, &b);
            then(&ui);
        });
    });
}

fn contact_from(
    record: &UserDirectoryRecord,
    nicknames: &std::collections::BTreeMap<String, String>,
) -> Contact {
    let published = record
        .profile
        .as_ref()
        .and_then(|p| p.display_name.clone().or_else(|| p.name.clone()))
        .unwrap_or_else(|| record.npub.clone());
    let nickname = nicknames
        .get(&record.account_id_hex)
        .cloned()
        .unwrap_or_default();
    let display = if nickname.is_empty() {
        published.clone()
    } else {
        nickname.clone()
    };
    // Avatar identity stays tied to the *published* name so a private
    // nickname doesn't shift the gradient/initials the user already knows
    // from chat views (which don't apply nicknames).
    let (a, b, init) = avatar_for(&published);
    // KeyPackages now publish to the NIP-65 outbox relays (no dedicated
    // key-package relay list since the upstream relay-list rework), so the
    // nip65 + inbox counts already cover the account's relays.
    let relays = record.relay_lists.nip65.relays.len() + record.relay_lists.inbox.relays.len();
    let nip05_verified = record
        .profile
        .as_ref()
        .and_then(|p| p.nip05.as_ref())
        .is_some();
    let picture_url = record.profile.as_ref().and_then(|p| p.picture.clone());
    let (picture, has_picture) = bind_cached_picture(picture_url.as_deref());
    Contact {
        name: s(&display),
        real_name: s(&published),
        nickname: s(&nickname),
        account_id: s(&record.account_id_hex),
        npub_full: s(&record.npub),
        npub_short: s(&shorten_npub(&record.npub)),
        av_a: a,
        av_b: b,
        av_initials: s(&init),
        verified: nip05_verified,
        online: false,
        relays: relays as i32,
        added: s(""),
        picture,
        has_picture,
    }
}

/// Split the new-chat modal's members textarea into individual npubs/hex
/// pubkeys. Accepts whitespace, comma, semicolon, or newline as separators.
/// No validation — the marmot runtime parses each entry and errors out on
/// invalid input, which we surface back to the user.
fn parse_member_list(raw: &str) -> Vec<String> {
    raw.split(|c: char| c.is_whitespace() || c == ',' || c == ';')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn shorten_npub(npub: &str) -> String {
    if npub.len() <= 16 {
        return npub.to_string();
    }
    format!("{}…{}", &npub[..10], &npub[npub.len() - 6..])
}

// ─── Group members ─────────────────────────────────────────────────────

/// Process-wide record of which group is currently shown, so async group-avatar
/// decodes that finish after the user has switched chats don't paint the wrong
/// group's image into the header/panel.
fn active_group_slot() -> &'static Mutex<String> {
    use std::sync::OnceLock;
    static SLOT: OnceLock<Mutex<String>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(String::new()))
}

/// Push the admin group-settings surface (rename draft + group avatar) for the
/// active group. The avatar drives both the chat header and the members panel
/// via the `chat-group-*` root properties. For 1:1 chats the group avatar is
/// cleared (the header falls back to the peer avatar in `ChatMeta`).
fn push_group_settings_to_ui_from(
    ui: &DarkMatterLinux,
    backend: &Backend,
    group_hex: &str,
    rec: Option<&AppGroupRecord>,
    count: usize,
) {
    if count <= 2 || rec.is_none() {
        ui.set_chat_group_has_picture(false);
        ui.set_chat_group_picture(slint::Image::default());
        return;
    }
    let rec = rec.unwrap();
    let name = rec.profile.name.clone();
    let label = if name.is_empty() { group_hex } else { &name };
    let (a, b, init) = avatar_for(label);
    ui.set_chat_group_av_a(a);
    ui.set_chat_group_av_b(b);
    ui.set_chat_group_av_initials(s(&init));
    ui.set_group_rename_draft(s(&name));

    let image_hash = if rec.image.present && !rec.image.image_hash_hex.is_empty() {
        Some(rec.image.image_hash_hex.clone())
    } else {
        None
    };
    match image_hash {
        Some(hash) => {
            let key = format!("group-image:{hash}");
            if let Some(img) = cached_picture_image(&key) {
                ui.set_chat_group_picture(img);
                ui.set_chat_group_has_picture(true);
            } else {
                ui.set_chat_group_has_picture(false);
                ui.set_chat_group_picture(slint::Image::default());
                spawn_group_image_fetch(ui, backend, group_hex, key);
            }
        }
        None => {
            ui.set_chat_group_has_picture(false);
            ui.set_chat_group_picture(slint::Image::default());
        }
    }
}

/// Fetch + decrypt + decode the active group's avatar on the tokio runtime,
/// cache the RGBA under `cache_key`, then bind it on the UI thread — but only
/// if the user is still viewing this group.
fn spawn_group_image_fetch(
    ui: &DarkMatterLinux,
    backend: &Backend,
    group_hex: &str,
    cache_key: String,
) {
    let weak = ui.as_weak();
    let group_hex = group_hex.to_string();
    backend.fetch_group_image_async(&group_hex.clone(), move |result| {
        let bytes = match result {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[group-avatar] fetch failed: {e:#}");
                return;
            }
        };
        let pixels = match decode_avatar_pixels(&bytes) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[group-avatar] decode failed: {e}");
                return;
            }
        };
        picture_cache_put(cache_key, pixels.clone());
        let _ = slint::invoke_from_event_loop(move || {
            // Ignore if the user navigated away before the decode finished.
            let still_active = active_group_slot()
                .lock()
                .map(|slot| slot.eq_ignore_ascii_case(&group_hex))
                .unwrap_or(false);
            if !still_active {
                return;
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_chat_group_picture(rgba_to_slint_image(&pixels));
                ui.set_chat_group_has_picture(true);
            }
        });
    });
}

/// Everything the members panel needs from the backend, gathered OFF the UI
/// thread (`chats()` and `group_members()` hit sqlite, which can stall behind
/// sync writes or a slow disk).
struct MembersSnapshot {
    group_rec: Option<AppGroupRecord>,
    count: usize,
    viewer_is_admin: bool,
    admins: Vec<String>,
    members: Vec<AppGroupMemberRecord>,
}

fn fetch_members_snapshot(backend: &Backend, group_hex: &str) -> MembersSnapshot {
    let records = backend.chats().unwrap_or_default();
    let group_rec = records
        .iter()
        .find(|g| g.group_id_hex.eq_ignore_ascii_case(group_hex))
        .cloned();
    let admins = group_rec
        .as_ref()
        .map(|g| g.admin_policy.admins.clone())
        .unwrap_or_default();
    let me = &backend.account().account_id_hex;
    let viewer_is_admin = admins.iter().any(|a| a.eq_ignore_ascii_case(me));
    let members = backend.group_members(group_hex).unwrap_or_else(|e| {
        eprintln!("[members] {e:#}");
        Vec::new()
    });
    MembersSnapshot {
        count: backend.group_member_count(group_hex),
        group_rec,
        viewer_is_admin,
        admins,
        members,
    }
}

/// Fetch the members snapshot on the backend runtime, then apply it on the
/// UI thread. Marks `group_hex` as the active group slot immediately so a
/// stale completion (user already switched groups) is dropped at apply time.
fn push_group_members_to_ui_async(ui: &DarkMatterLinux, backend: &Arc<Backend>, group_hex: &str) {
    if let Ok(mut slot) = active_group_slot().lock() {
        *slot = group_hex.to_string();
    }
    let b = backend.clone();
    let group_hex = group_hex.to_string();
    let weak = ui.as_weak();
    backend.tokio_handle().spawn(async move {
        let snap = fetch_members_snapshot(&b, &group_hex);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            let still_active = active_group_slot()
                .lock()
                .map(|slot| slot.eq_ignore_ascii_case(&group_hex))
                .unwrap_or(false);
            if !still_active {
                return;
            }
            push_group_members_to_ui_from(&ui, &b, &group_hex, snap);
        });
    });
}

fn push_group_members_to_ui_from(
    ui: &DarkMatterLinux,
    backend: &Backend,
    group_hex: &str,
    snap: MembersSnapshot,
) {
    push_group_settings_to_ui_from(ui, backend, group_hex, snap.group_rec.as_ref(), snap.count);
    let count = snap.count;
    let is_group = count > 2;
    let can_show_members = count >= 2;
    ui.set_chat_is_group(is_group);
    ui.set_chat_can_show_members(can_show_members);
    ui.set_chat_member_count(count as i32);
    if !can_show_members {
        ui.set_chat_members(model(Vec::new()));
        ui.set_show_chat_members(false);
        ui.set_chat_can_add_members(false);
        return;
    }
    let viewer_is_admin = snap.viewer_is_admin;
    ui.set_chat_can_add_members(viewer_is_admin);
    let admins = snap.admins;
    let my_id = backend.account().account_id_hex.clone();
    // Build (row, picture_url) pairs so we can spawn per-member image fetches
    // after the initial paint. URLs are looked up once here and shipped to
    // the worker tasks; the model itself only carries the decoded image.
    let mut pairs: Vec<(GroupMember, Option<String>)> = snap
        .members
        .iter()
        .map(|m| group_member_from(backend, m, &my_id, &admins, viewer_is_admin))
        .collect();
    pairs.sort_by(|(a, _), (b, _)| match (a.is_self, b.is_self) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
    let rows: Vec<GroupMember> = pairs.iter().map(|(r, _)| r.clone()).collect();
    ui.set_chat_members(model(rows));

    // Spawn fetches for any row that has a picture URL and isn't already
    // bound to a cached image. We key the row back up by `npub_short` since
    // that's the stable per-member field we already render.
    for (row, url) in pairs.into_iter() {
        let Some(url) = url else { continue };
        if row.has_picture {
            continue;
        }
        spawn_member_picture_fetch(ui, backend, row.npub_short.to_string(), url);
    }
}

/// Spawn async avatar fetches for the open chat's incoming senders. When a
/// picture decodes, every bubble from that sender (keyed by `sender-id`) gets
/// the image bound in place — no full rebuild. Mirrors the members pipeline.
fn spawn_message_avatar_fetches(
    ui: &DarkMatterLinux,
    backend: &Backend,
    msgs: &[AppMessageRecord],
) {
    let my_id = backend.account().account_id_hex.clone();
    let profiles = build_sender_profiles(backend, msgs, &my_id);
    let mut seen = std::collections::HashSet::new();
    let targets: Vec<(String, String)> = profiles
        .iter()
        .filter_map(|(sender, (_, url))| {
            let url = url.as_deref().map(str::trim).filter(|u| !u.is_empty())?;
            if picture_cache_has(url) || !seen.insert(sender.clone()) {
                return None;
            }
            Some((sender.clone(), url.to_string()))
        })
        .collect();
    for (sender_id, url) in targets {
        let weak = ui.as_weak();
        backend.tokio_handle().spawn(async move {
            let Some(pixels) = fetch_picture_pixels(&url).await else {
                return;
            };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    update_bubble_pictures(&ui, &sender_id, &pixels);
                }
            });
        });
    }
}

/// Bind a decoded picture onto every incoming bubble from `sender_id` in the
/// currently-open chat. Outgoing rows are skipped (they paint `my-picture`).
fn update_bubble_pictures(ui: &DarkMatterLinux, sender_id: &str, pixels: &PicturePixels) {
    let idx = ui.get_active_chat() as usize;
    let outer = ui.get_chats_messages();
    let Some(outer_vm) = outer
        .as_any()
        .downcast_ref::<VecModel<ModelRc<ChatMessage>>>()
    else {
        return;
    };
    let Some(inner) = outer_vm.row_data(idx) else {
        return;
    };
    let Some(vm) = inner.as_any().downcast_ref::<VecModel<ChatMessage>>() else {
        return;
    };
    let img = rgba_to_slint_image(pixels);
    for i in 0..vm.row_count() {
        let Some(mut row) = vm.row_data(i) else {
            continue;
        };
        if row.outgoing || row.sender_id.as_str() != sender_id {
            continue;
        }
        row.picture = img.clone();
        row.has_picture = true;
        vm.set_row_data(i, row);
    }
}

/// Spawn async avatar fetches for the 1:1 peers in the chat list. On decode the
/// matching `ChatMeta` row (keyed by its `npub`) gets the picture bound.
fn spawn_chat_list_avatar_fetches(ui: &DarkMatterLinux, backend: &Arc<Backend>) {
    // The enumeration itself reads `chats()` (sqlite) — runtime, not UI thread.
    let weak_outer = ui.as_weak();
    let b = backend.clone();
    backend.tokio_handle().spawn(async move {
        let chats = match b.chats() {
            Ok(c) => c,
            Err(_) => return,
        };
        for record in chats {
            let Some(peer) = b.direct_chat_peer(&record.group_id_hex) else {
                continue;
            };
            let (_, url) = b.account_name_and_picture(&peer);
            let Some(url) = url.map(|u| u.trim().to_string()).filter(|u| !u.is_empty()) else {
                continue;
            };
            if picture_cache_has(&url) {
                continue;
            }
            let npub = format!("mls:0x{}", short_hex(&record.group_id_hex));
            let weak = weak_outer.clone();
            b.tokio_handle().spawn(async move {
                let Some(pixels) = fetch_picture_pixels(&url).await else {
                    return;
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        update_chat_picture(&ui, &npub, &pixels);
                    }
                });
            });
        }
    });
}

/// Bind a decoded picture onto the chat-list row identified by `npub`.
fn update_chat_picture(ui: &DarkMatterLinux, npub: &str, pixels: &PicturePixels) {
    let chats = ui.get_chats();
    let Some(vm) = chats.as_any().downcast_ref::<VecModel<ChatMeta>>() else {
        return;
    };
    let img = rgba_to_slint_image(pixels);
    for i in 0..vm.row_count() {
        let Some(mut row) = vm.row_data(i) else {
            continue;
        };
        if row.npub.as_str() != npub {
            continue;
        }
        row.picture = img.clone();
        row.has_picture = true;
        vm.set_row_data(i, row);
        break;
    }
}

/// Queue async fetches for contact-list avatars whose picture URL isn't in
/// the cache yet. Mirrors [`spawn_chat_list_avatar_fetches`].
fn spawn_contact_avatar_fetches(ui: &DarkMatterLinux, backend: &Arc<Backend>) {
    // The enumeration itself reads `follow_list()` (sqlite) — runtime only.
    let weak_outer = ui.as_weak();
    let b = backend.clone();
    backend.tokio_handle().spawn(async move {
        let records = match b.follow_list() {
            Ok(v) => v,
            Err(_) => return,
        };
        for record in records {
            let Some(url) = record
                .profile
                .as_ref()
                .and_then(|p| p.picture.clone())
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty())
            else {
                continue;
            };
            if picture_cache_has(&url) {
                continue;
            }
            let account_id = record.account_id_hex.clone();
            let weak = weak_outer.clone();
            b.tokio_handle().spawn(async move {
                let Some(pixels) = fetch_picture_pixels(&url).await else {
                    return;
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        update_contact_picture(&ui, &account_id, &pixels);
                    }
                });
            });
        }
    });
}

/// Bind a decoded picture onto the contact row identified by `account_id`.
fn update_contact_picture(ui: &DarkMatterLinux, account_id: &str, pixels: &PicturePixels) {
    let contacts = ui.get_contacts();
    let Some(vm) = contacts.as_any().downcast_ref::<VecModel<Contact>>() else {
        return;
    };
    let img = rgba_to_slint_image(pixels);
    for i in 0..vm.row_count() {
        let Some(mut row) = vm.row_data(i) else {
            continue;
        };
        if !row.account_id.as_str().eq_ignore_ascii_case(account_id) {
            continue;
        }
        row.picture = img.clone();
        row.has_picture = true;
        vm.set_row_data(i, row);
        break;
    }
}

/// Queue async fetches for archived-chat avatars (1:1 peers only) whose
/// picture URL isn't in the cache yet.
fn spawn_archived_avatar_fetches(ui: &DarkMatterLinux, backend: &Arc<Backend>) {
    // The enumeration itself reads `archived_chats()` (sqlite) — runtime only.
    let weak_outer = ui.as_weak();
    let b = backend.clone();
    backend.tokio_handle().spawn(async move {
        let records = match b.archived_chats() {
            Ok(v) => v,
            Err(_) => return,
        };
        for record in records {
            let Some(peer) = b.direct_chat_peer(&record.group_id_hex) else {
                continue;
            };
            let Some(url) = b
                .account_picture_url(&peer)
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty())
            else {
                continue;
            };
            if picture_cache_has(&url) {
                continue;
            }
            let group_id = format!("mls:0x{}", short_hex(&record.group_id_hex));
            let weak = weak_outer.clone();
            b.tokio_handle().spawn(async move {
                let Some(pixels) = fetch_picture_pixels(&url).await else {
                    return;
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        update_archived_picture(&ui, &group_id, &pixels);
                    }
                });
            });
        }
    });
}

/// Bind a decoded picture onto the archived row identified by `group_id`.
fn update_archived_picture(ui: &DarkMatterLinux, group_id: &str, pixels: &PicturePixels) {
    let archived = ui.get_archived_chats();
    let Some(vm) = archived.as_any().downcast_ref::<VecModel<ArchivedChat>>() else {
        return;
    };
    let img = rgba_to_slint_image(pixels);
    for i in 0..vm.row_count() {
        let Some(mut row) = vm.row_data(i) else {
            continue;
        };
        if row.group_id.as_str() != group_id {
            continue;
        }
        row.picture = img.clone();
        row.has_picture = true;
        vm.set_row_data(i, row);
        break;
    }
}

fn spawn_member_picture_fetch(
    ui: &DarkMatterLinux,
    backend: &Backend,
    npub_short: String,
    url: String,
) {
    let weak = ui.as_weak();
    backend.tokio_handle().spawn(async move {
        let pixels = match fetch_picture_pixels(&url).await {
            Some(p) => p,
            None => return,
        };
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            update_member_picture(&ui, &npub_short, &pixels);
        });
    });
}

fn update_member_picture(ui: &DarkMatterLinux, npub_short: &str, pixels: &PicturePixels) {
    let members = ui.get_chat_members();
    let Some(vm) = members.as_any().downcast_ref::<VecModel<GroupMember>>() else {
        return;
    };
    for i in 0..vm.row_count() {
        let Some(mut row) = vm.row_data(i) else {
            continue;
        };
        if row.npub_short.as_str() != npub_short {
            continue;
        }
        let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
            &pixels.rgba,
            pixels.w,
            pixels.h,
        );
        row.picture = slint::Image::from_rgba8(buffer);
        row.has_picture = true;
        vm.set_row_data(i, row);
        break;
    }
}

/// Shared async fetch + decode for an arbitrary image URL. Returns the
/// raw RGBA pixels (Send) so the caller can shuttle them across the event
/// loop and build a `slint::Image` on the UI thread. Hits the same
/// process-wide cache as `fetch_profile_picture`.
async fn fetch_picture_pixels(url: &str) -> Option<PicturePixels> {
    if let Some(p) = picture_cache_get(url) {
        return Some(p);
    }
    let bytes = match reqwest::get(url).await {
        Ok(resp) => match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[avatar] download failed for {url}: {e}");
                return None;
            }
        },
        Err(e) => {
            eprintln!("[avatar] request failed for {url}: {e}");
            return None;
        }
    };
    let pixels = match decode_avatar_pixels(&bytes) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[avatar] decode failed for {url}: {e}");
            return None;
        }
    };
    picture_cache_put(url.to_string(), pixels.clone());
    Some(pixels)
}

fn group_member_from(
    backend: &Backend,
    record: &AppGroupMemberRecord,
    my_account_id_hex: &str,
    admins: &[String],
    viewer_is_admin: bool,
) -> (GroupMember, Option<String>) {
    let is_self = record.member_id_hex.eq_ignore_ascii_case(my_account_id_hex);
    let is_admin = admins
        .iter()
        .any(|a| a.eq_ignore_ascii_case(&record.member_id_hex));
    // The viewer can hand out admin only to other members who aren't admins yet.
    let can_promote = viewer_is_admin && !is_self && !is_admin;
    // Demote another admin, or step down from one's own admin role.
    let can_demote = viewer_is_admin && is_admin && !is_self;
    let can_self_demote = viewer_is_admin && is_admin && is_self;
    let mut name = backend.account_display_name(&record.member_id_hex);
    if let Some(label) = record.account.as_ref().filter(|s| !s.is_empty())
        && name.starts_with("0x")
    {
        name = label.clone();
    }
    let npub =
        npub_for_account_id(&record.member_id_hex).unwrap_or_else(|_| record.member_id_hex.clone());
    let (a, b, init) = avatar_for(&name);
    let picture_url = backend
        .account_picture_url(&record.member_id_hex)
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty());
    // If the cache already has pixels for this URL, bind them now so the
    // row paints with the image on the first frame (no flash-of-initials).
    let (picture_img, has_picture) = bind_cached_picture(picture_url.as_deref());
    let row = GroupMember {
        name: s(&name),
        npub_short: s(&shorten_npub(&npub)),
        av_a: a,
        av_b: b,
        av_initials: s(&init),
        is_self,
        picture: picture_img,
        has_picture,
        member_id: s(&record.member_id_hex),
        is_admin,
        can_promote,
        can_demote,
        can_self_demote,
    };
    (row, picture_url)
}

/// Largest edge we keep for decoded avatar/group pictures. They render at
/// ≤160px logical (profile page), so 512px covers hidpi with headroom while
/// turning a multi-megapixel upload into a ≤1MB RGBA buffer — smaller memcpys
/// on every cache read and a far smaller GPU texture. Chat *attachments* are
/// not capped (the lightbox shows them full size).
const MAX_AVATAR_DECODE_PX: u32 = 512;

/// Decode image bytes to RGBA, downscaling to [`MAX_AVATAR_DECODE_PX`].
fn decode_avatar_pixels(bytes: &[u8]) -> Result<PicturePixels, image::ImageError> {
    let img = image::load_from_memory(bytes)?;
    let img = if img.width() > MAX_AVATAR_DECODE_PX || img.height() > MAX_AVATAR_DECODE_PX {
        img.thumbnail(MAX_AVATAR_DECODE_PX, MAX_AVATAR_DECODE_PX)
    } else {
        img
    };
    let img = img.to_rgba8();
    let (w, h) = img.dimensions();
    Ok(PicturePixels {
        w,
        h,
        rgba: img.into_raw(),
    })
}

fn rgba_to_slint_image(pixels: &PicturePixels) -> slint::Image {
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        &pixels.rgba,
        pixels.w,
        pixels.h,
    );
    slint::Image::from_rgba8(buffer)
}

// ─── Archived ──────────────────────────────────────────────────────────

/// Archived-list state gathered OFF the UI thread (sqlite reads).
struct ArchivedSnapshot {
    records: Vec<AppGroupRecord>,
    /// Parallel to `records`.
    latest: Vec<Option<AppMessageRecord>>,
}

fn fetch_archived_snapshot(backend: &Backend) -> Option<ArchivedSnapshot> {
    let records = match backend.archived_chats() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[backend] archived_chats failed: {e:#}");
            return None;
        }
    };
    let latest = records
        .iter()
        .map(|r| backend.latest_message(&r.group_id_hex))
        .collect();
    Some(ArchivedSnapshot { records, latest })
}

fn refresh_archived_from(
    backend: &Backend,
    snap: &ArchivedSnapshot,
    archived: &ModelRc<ArchivedChat>,
    archived_group_ids: &Arc<Mutex<Vec<String>>>,
) {
    let my_id = backend.account().account_id_hex.clone();
    let mut ids = Vec::with_capacity(snap.records.len());
    let rows: Vec<ArchivedChat> = snap
        .records
        .iter()
        .zip(snap.latest.iter())
        .map(|(r, latest)| {
            ids.push(r.group_id_hex.clone());
            archived_from(r, latest.as_ref(), &my_id, backend)
        })
        .collect();
    *archived_group_ids.lock().unwrap() = ids;
    if let Some(vm) = archived.as_any().downcast_ref::<VecModel<ArchivedChat>>() {
        vm.set_vec(rows);
    }
}

/// Fetch + apply the archived list (+ avatar fetches) off the UI thread.
fn refresh_archived_async(
    ui: &DarkMatterLinux,
    backend: &Arc<Backend>,
    archived_group_ids: &Arc<Mutex<Vec<String>>>,
) {
    let weak = ui.as_weak();
    let b = backend.clone();
    let archived_group_ids = archived_group_ids.clone();
    backend.tokio_handle().spawn(async move {
        let Some(snap) = fetch_archived_snapshot(&b) else {
            return;
        };
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            let archived = ui.get_archived_chats();
            refresh_archived_from(&b, &snap, &archived, &archived_group_ids);
            spawn_archived_avatar_fetches(&ui, &b);
        });
    });
}

fn archived_from(
    record: &AppGroupRecord,
    last_message: Option<&AppMessageRecord>,
    my_account_id_hex: &str,
    backend: &Backend,
) -> ArchivedChat {
    let name = if record.profile.name.is_empty() {
        record.group_id_hex.clone()
    } else {
        record.profile.name.clone()
    };
    let (a, b, init) = avatar_for(&name);
    let (last_msg, last_date) = match last_message {
        Some(m) => {
            let mine = m.sender.eq_ignore_ascii_case(my_account_id_hex);
            let prefix = if mine {
                "You: ".to_string()
            } else {
                String::new()
            };
            (
                format!("{prefix}{}", m.plaintext),
                format_chat_stamp(m.recorded_at),
            )
        }
        None => (record.profile.description.clone(), String::new()),
    };
    // Archived 1:1 chats keep the peer's profile picture; groups stay on the
    // gradient (group images are an MLS component, not a public URL).
    let picture_url = backend
        .direct_chat_peer(&record.group_id_hex)
        .and_then(|peer| backend.account_picture_url(&peer));
    let (picture, has_picture) = bind_cached_picture(picture_url.as_deref());
    ArchivedChat {
        name: s(&name),
        last_msg: s(&last_msg),
        last_date: s(&last_date),
        av_a: a,
        av_b: b,
        av_initials: s(&init),
        members: backend.group_member_count(&record.group_id_hex) as i32,
        group_id: s(&format!("mls:0x{}", short_hex(&record.group_id_hex))),
        picture,
        has_picture,
    }
}

// ─── Per-chat live message watcher ─────────────────────────────────────

/// Attach a watcher that appends new messages into the inner messages model
/// for the currently-open chat. Caller is responsible for aborting the
/// returned `JoinHandle` when the user switches chats.
fn install_message_watcher(
    backend: &Backend,
    weak: Weak<DarkMatterLinux>,
    backend_cell: Arc<Mutex<Option<Arc<Backend>>>>,
    pending_state: Arc<Mutex<PendingState>>,
    group_hex: String,
    chat_idx: usize,
    _my_id: String,
) -> JoinHandle<()> {
    let group_hex_for_filter = group_hex.clone();
    backend.watch_messages(&group_hex, move |update| {
        let received = update.message();
        let event_group = hex::encode(received.group_id.as_slice());
        if event_group != group_hex_for_filter {
            return;
        }
        // Three interesting wire kinds. Each one becomes a surgical model
        // update so neighbouring bubbles don't remount.
        let kind = received.kind;
        if !matches!(kind, 9 | 7 | 5 | 1009) {
            return;
        }
        let weak = weak.clone();
        let backend_cell = backend_cell.clone();
        let pending_state = pending_state.clone();
        let group_hex_inner = group_hex_for_filter.clone();
        let msg_id = received.message_id_hex.clone();
        let target_id_for_reaction: Option<String> = if kind == 7 || kind == 5 || kind == 1009 {
            received
                .tags
                .iter()
                .find(|t| t.len() >= 2 && t[0] == "e")
                .map(|t| t[1].clone())
        } else {
            None
        };
        // This callback runs on the backend runtime. Read the window snapshot
        // HERE — the event-loop closure below must never touch sqlite (it can
        // stall behind sync writes or a slow disk and freeze the UI).
        let Some(b) = backend_cell.lock().unwrap().clone() else {
            return;
        };
        let all = b
            .messages(&group_hex_inner, Some(msg_window_for(&group_hex_inner)))
            .unwrap_or_default();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            let overlay = pending_state.lock().unwrap();
            let chats_messages = ui.get_chats_messages();

            match kind {
                9 => {
                    // Chat message echo. If the row already exists (because
                    // we just reconciled our own send), do nothing. Otherwise
                    // append it surgically — no full rebuild.
                    let my_id = b.account().account_id_hex.clone();
                    let my_label = my_avatar_label(&b, &my_id);
                    let Some(rec) = all.iter().find(|m| m.message_id_hex == msg_id).cloned() else {
                        return;
                    };
                    let row = build_one_message_row(
                        &rec,
                        &all,
                        &my_id,
                        &my_label,
                        &group_hex_inner,
                        &overlay,
                        &b,
                    );
                    let pushed = with_inner_messages(&chats_messages, chat_idx, |vm| {
                        if find_message_row(vm, &msg_id).is_none() {
                            push_message_grouped(vm, row);
                            true
                        } else {
                            false
                        }
                    })
                    .unwrap_or(false);
                    // A brand-new sender's avatar may not be cached yet; fetch
                    // it so the freshly-appended bubble fills in.
                    if pushed && !rec.sender.eq_ignore_ascii_case(&my_id) {
                        drop(overlay);
                        spawn_message_avatar_fetches(&ui, &b, &all);
                    }
                }
                7 | 5 | 1009 => {
                    // Reaction, delete, or edit — surgical refresh of the
                    // target row. For an edit the snapshot now carries the
                    // kind-1009, so the rebuilt row picks up the new text.
                    let Some(target) = target_id_for_reaction else {
                        return;
                    };
                    refresh_one_message_row_from(
                        &b,
                        &overlay,
                        &chats_messages,
                        chat_idx,
                        &group_hex_inner,
                        &target,
                        &all,
                    );
                }
                _ => {}
            }
        });
    })
}
