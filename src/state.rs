use crate::*;

#[derive(Clone)]
pub(crate) struct PendingSend {
    // Local-only id so retry/failure can find the entry. Carried into the
    // bubble's `message_id` so the retry callback can resolve back here.
    pub(crate) temp_id: String,
    pub(crate) text: String,
    pub(crate) failed: bool,
    // When `Some`, this send is a reply — sent via `reply_to_message` so the
    // event carries `e`+`q` tags. The triple is (parent_id_hex, author_label,
    // preview_text) — same values we render in the chip + quoted block.
    pub(crate) reply_to: Option<(String, String, String)>,
    // Media upload + send. Empty for a plain text send; one entry for a single
    // attachment (chip/image preview); 2+ for an album (rendered as a grid).
    // The optimistic bubble renders straight from the local previews while the
    // encrypt+blossom+publish round-trip resolves.
    pub(crate) media: Vec<PendingMedia>,
    // Armed message effect (Telegram-style burst), 0 = none. Plays once on the
    // optimistic outgoing row; the wire body carries the matching marker so the
    // recipient replays it. Attachment sends leave this 0.
    pub(crate) effect: i32,
}

#[derive(Clone)]
pub(crate) struct PendingMedia {
    pub(crate) file_name: String,
    pub(crate) media_type: String,
    pub(crate) size_bytes: u64,
    pub(crate) is_image: bool,
    pub(crate) is_video: bool,
    pub(crate) is_audio: bool,
    // Local pixels for instant image preview while the upload is in flight.
    // None for non-image attachments.
    pub(crate) local_preview: Option<PicturePixels>,
}

/// One attachment queued in the composer (paperclip picker or clipboard
/// paste) but not yet sent. The bytes stay Rust-side; the UI only gets a
/// `StagedAttachment` chip row built by [`refresh_staged_ui`]. Nothing
/// uploads until the user presses Send — the visible chips *are* the
/// confirmation step.
#[derive(Clone)]
pub(crate) struct StagedFile {
    pub(crate) file_name: String,
    pub(crate) media_type: String,
    pub(crate) bytes: Vec<u8>,
    pub(crate) is_image: bool,
    // Full-resolution decode, reused as the optimistic bubble preview and
    // seeded into the attachment image cache once the upload confirms.
    pub(crate) preview: Option<PicturePixels>,
    // Small (≤96px) decode for the chip thumbnail, so rebuilding the chip
    // model never copies full screenshots around.
    pub(crate) thumb: Option<PicturePixels>,
}

#[derive(Clone)]
pub(crate) enum PendingReactionOp {
    /// I just clicked an emoji on a confirmed message — add a chip with
    /// `mine: true` unless the snapshot already shows my reaction.
    Add(String),
    /// I just unreacted — drop the `mine` flag and count from any chips on
    /// this target while the network catches up.
    Remove,
}

#[derive(Default)]
pub(crate) struct PendingState {
    /// group_hex → ordered list of pending outgoing messages. Append-only;
    /// entries are removed (or marked failed) when the send resolves.
    pub(crate) sends: HashMap<String, Vec<PendingSend>>,
    /// (group_hex, target_message_id_hex) → my latest pending reaction op
    /// on that target. Only one op per target at a time (the most recent
    /// click wins).
    pub(crate) reactions: HashMap<(String, String), PendingReactionOp>,
    /// (group_hex, target_message_id_hex) → the replacement text of my
    /// not-yet-confirmed edit of that message. Mirrors `reactions`: a single
    /// in-flight op per target; cleared when the kind-1009 send resolves.
    pub(crate) edits: HashMap<(String, String), String>,
    /// (group_hex, target_message_id_hex) of my not-yet-confirmed "delete for
    /// everyone" retractions. The target renders as a tombstone immediately;
    /// the entry is cleared when the kind-5 send resolves (on ack the snapshot
    /// carries the delete; on failure the row reverts).
    pub(crate) deletes: HashSet<(String, String)>,
}

impl PendingState {
    pub(crate) fn add_send(&mut self, group_hex: &str, send: PendingSend) {
        self.sends
            .entry(group_hex.to_string())
            .or_default()
            .push(send);
    }
    pub(crate) fn drop_send(&mut self, group_hex: &str, temp_id: &str) {
        if let Some(v) = self.sends.get_mut(group_hex) {
            v.retain(|p| p.temp_id != temp_id);
        }
    }
    pub(crate) fn mark_send_failed(&mut self, group_hex: &str, temp_id: &str) {
        if let Some(v) = self.sends.get_mut(group_hex) {
            for p in v.iter_mut() {
                if p.temp_id == temp_id {
                    p.failed = true;
                }
            }
        }
    }
    pub(crate) fn find_send(&self, group_hex: &str, temp_id: &str) -> Option<PendingSend> {
        self.sends
            .get(group_hex)
            .and_then(|v| v.iter().find(|p| p.temp_id == temp_id).cloned())
    }
}

// Monotonic temp-id source. Survives the lifetime of the process; we only
// need uniqueness within a session.
pub(crate) fn next_temp_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let v = N.fetch_add(1, Ordering::Relaxed);
    format!("pending:{v}")
}

// ─── Delete-for-me (local-only hidden messages) ──────────────────────────────
//
// "Delete for me" never touches the wire — it just hides a message id from this
// client, for the account that hid it. The durable store is
// `Settings.hidden_messages_by_account`; this process-wide map (account hex →
// hidden ids) is the fast in-memory view consulted by `is_visible_chat_message`
// (a free fn with no settings handle). `hidden_account()` names the account
// whose set the renderer currently consults — it follows the active account,
// so a hide on one account never leaks to another on the same machine.
pub(crate) fn hidden_messages() -> &'static Mutex<HashMap<String, HashSet<String>>> {
    static S: OnceLock<Mutex<HashMap<String, HashSet<String>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The account hex whose hidden set the renderer currently consults. Set at
/// boot and on every account switch, before the chat models are rebuilt.
pub(crate) fn hidden_account() -> &'static Mutex<String> {
    static S: OnceLock<Mutex<String>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(String::new()))
}

/// Point the renderer at `account_hex`'s hidden set (called on boot + switch).
pub(crate) fn hidden_set_account(account_hex: &str) {
    if let Ok(mut a) = hidden_account().lock() {
        *a = account_hex.to_ascii_lowercase();
    }
}

/// Seed `account_hex`'s in-memory hidden set from persisted settings (idempotent).
pub(crate) fn hidden_init(account_hex: &str, ids: impl IntoIterator<Item = String>) {
    if let Ok(mut m) = hidden_messages().lock() {
        m.entry(account_hex.to_ascii_lowercase())
            .or_default()
            .extend(ids);
    }
}

/// Mark a message hidden for `account_hex` in the in-memory map. Returns true if
/// it wasn't already hidden (so the caller knows to persist + rebuild).
pub(crate) fn hidden_insert(account_hex: &str, message_id: &str) -> bool {
    hidden_messages()
        .lock()
        .map(|mut m| {
            m.entry(account_hex.to_ascii_lowercase())
                .or_default()
                .insert(message_id.to_string())
        })
        .unwrap_or(false)
}

pub(crate) fn is_hidden_message(message_id: &str) -> bool {
    let account = hidden_account()
        .lock()
        .map(|a| a.clone())
        .unwrap_or_default();
    hidden_messages()
        .lock()
        .map(|m| m.get(&account).is_some_and(|s| s.contains(message_id)))
        .unwrap_or(false)
}

/// Pre-upgrade global hides, stashed at startup (when the boot account isn't
/// known yet) and folded into the boot account's set once it is. `take` drains
/// it so the fold runs once per boot.
pub(crate) fn hidden_legacy_stash() -> &'static Mutex<Vec<String>> {
    static S: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Vec::new()))
}

pub(crate) fn hidden_stash_legacy(ids: Vec<String>) {
    if let Ok(mut s) = hidden_legacy_stash().lock() {
        *s = ids;
    }
}

pub(crate) fn hidden_take_legacy() -> Vec<String> {
    hidden_legacy_stash()
        .lock()
        .map(|mut s| std::mem::take(&mut *s))
        .unwrap_or_default()
}

// ─── Durable offline send queue ────────────────────────────────────────────
//
// The optimistic overlay above lives only in RAM. These process-wide handles
// add the missing durability + auto-flush-on-reconnect: see `offline_queue.rs`
// for the encrypted on-disk store. The *disk* queue is the source of truth for
// any (re)dispatch — the overlay is just what's rendered.

// (HashSet / OnceLock / atomics are re-exported at the crate root above.)

/// temp_ids whose send is currently in flight (dispatched, not yet resolved).
/// The reconnect flush skips these so a send can't be dispatched twice
/// concurrently. Entries are inserted at dispatch and removed when the op
/// resolves (ack or error), on whichever thread resolves it.
pub(crate) fn offline_inflight() -> &'static Mutex<HashSet<String>> {
    static S: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(crate) fn offline_inflight_insert(temp_id: &str) {
    if let Ok(mut s) = offline_inflight().lock() {
        s.insert(temp_id.to_string());
    }
}

pub(crate) fn offline_inflight_remove(temp_id: &str) {
    if let Ok(mut s) = offline_inflight().lock() {
        s.remove(temp_id);
    }
}

pub(crate) fn offline_inflight_contains(temp_id: &str) -> bool {
    offline_inflight()
        .lock()
        .map(|s| s.contains(temp_id))
        .unwrap_or(false)
}

/// Set by the background connectivity watcher when there's queued work to (re)try
/// — on first boot-ready and on every offline→online relay transition. The UI
/// timer consumes it and calls `flush_now`.
pub(crate) fn offline_flush_requested() -> &'static AtomicBool {
    static B: AtomicBool = AtomicBool::new(false);
    &B
}

/// Last-known connected relay count, published by the watcher thread so the
/// UI-thread flush can decide whether to dispatch (online) or only render the
/// queued bubbles (offline) without itself blocking on `relay_health`.
pub(crate) fn offline_last_connected() -> &'static AtomicUsize {
    static N: AtomicUsize = AtomicUsize::new(0);
    &N
}

/// Seal `send` into the durable queue using the session vault, if it's unlocked.
/// A no-op (best-effort) when the vault handle isn't present.
pub(crate) fn offline_persist(
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
pub(crate) fn looks_already_sent(
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

pub(crate) fn with_active_recorder<R>(f: impl FnOnce(&mut Option<audio::AudioRecorder>) -> R) -> R {
    ACTIVE_AUDIO_RECORDER.with(|r| f(&mut r.borrow_mut()))
}

pub(crate) fn with_active_player<R>(f: impl FnOnce(&mut Option<audio::AudioPlayer>) -> R) -> R {
    ACTIVE_AUDIO_PLAYER.with(|p| f(&mut p.borrow_mut()))
}

/// Start instant of the current recording, shared with the timer thread.
pub(crate) fn recording_start() -> &'static Mutex<Option<std::time::Instant>> {
    use std::sync::OnceLock;
    static S: OnceLock<Mutex<Option<std::time::Instant>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

/// The message id of the currently-playing voice message.
pub(crate) fn current_audio_message_id() -> &'static Mutex<Option<String>> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(None))
}

/// Last-known playback progress per message id (0..1). Kept so rows that
/// scroll out and back in show the correct progress without re-querying the
/// player.
pub(crate) fn audio_progress() -> &'static Mutex<HashMap<String, f32>> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<HashMap<String, f32>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Duration label per audio message id (e.g. "0:42"), captured the first
/// time the clip is decoded.
pub(crate) fn audio_meta() -> &'static Mutex<HashMap<String, String>> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn rgb(hex: u32) -> Color {
    Color::from_rgb_u8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

pub(crate) fn s(v: &str) -> SharedString {
    v.into()
}

/// Gate for setting a brand-new vault password. This password is the only thing
/// protecting every stored secret, and there is no recovery — so we require a
/// minimum length and a matching confirmation.
pub(crate) fn validate_new_password(pw: &str, confirm: &str) -> Result<(), String> {
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
pub(crate) struct ErrorCopySnapshot {
    pub(crate) invalid_key: String,
    pub(crate) network: String,
    pub(crate) sync: String,
    pub(crate) backend: String,
    pub(crate) switch_account: String,
    pub(crate) accept: String,
    pub(crate) block: String,
    pub(crate) archive: String,
    pub(crate) unarchive: String,
    pub(crate) send: String,
    pub(crate) edit: String,
    pub(crate) react: String,
    pub(crate) unreact: String,
    pub(crate) kp_publish: String,
    pub(crate) kp_rotate: String,
    pub(crate) kp_refresh: String,
    pub(crate) republish: String,
    pub(crate) add_account: String,
    pub(crate) create_chat: String,
    pub(crate) add_contact: String,
    pub(crate) add_member: String,
    pub(crate) group_settings: String,
    pub(crate) group_image: String,
    pub(crate) save_profile: String,
    pub(crate) upload_picture: String,
    pub(crate) generic: String,
    pub(crate) not_connected: String,
    pub(crate) relay_already_listed: String,
    pub(crate) save_relays_failed: String,
    pub(crate) relay_added: String,
    pub(crate) relay_removed: String,
    pub(crate) republishing: String,
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

pub(crate) fn error_copy_cell() -> &'static Mutex<ErrorCopySnapshot> {
    static C: OnceLock<Mutex<ErrorCopySnapshot>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(ErrorCopySnapshot::default()))
}

/// Snapshot the localized `ErrorCopy` strings off the Slint global into the
/// process-global cache. MUST be called on the UI/event-loop thread (it reads
/// Slint property getters). Call at startup and after every locale change so
/// worker-thread error copy follows the active language.
pub(crate) fn refresh_error_copy(ui: &DarkMatterLinux) {
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
pub(crate) fn error_copy() -> ErrorCopySnapshot {
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
pub(crate) fn friendly_error(op: &str, e: &anyhow::Error) -> String {
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

pub(crate) fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(v))
}

/// Recompute the breadcrumb from the UI's own models. Same effect as the
/// `refresh_breadcrumb` closure in `main`, but callable from `Send` completion
/// closures that can't capture the model handles.
pub(crate) fn refresh_breadcrumb_now(ui: &DarkMatterLinux) {
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

pub(crate) fn breadcrumb(
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
pub(crate) enum Page {
    Chats = 0,
    Contacts = 1,
    Archived = 2,
    Keys = 3,
    Settings = 4,
    Profile = 5,
}

// Master list of palette actions. Each has an id (used by Rust to dispatch),
// a label (shown), a group header, and an optional keyboard hint chip.
pub(crate) fn all_palette_actions() -> Vec<PaletteAction> {
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

pub(crate) fn filter_palette(all: &[PaletteAction], query: &str) -> Vec<PaletteAction> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return all.to_vec();
    }
    all.iter()
        .filter(|a| a.label.to_lowercase().contains(&q) || a.id.to_lowercase().contains(&q))
        .cloned()
        .collect()
}

pub(crate) fn normalize_locale(code: &str) -> &'static str {
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

pub(crate) fn normalize_theme_mode(mode: &str) -> &'static str {
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

pub(crate) fn normalize_accent_color(color: &str) -> &'static str {
    match color {
        "ocean" => "ocean",
        "berry" => "berry",
        "coral" => "coral",
        "lavender" => "lavender",
        _ => "mint",
    }
}

pub(crate) fn accent_color_idx(color: &str) -> i32 {
    match color {
        "ocean" => 1,
        "berry" => 2,
        "coral" => 3,
        "lavender" => 4,
        _ => 0,
    }
}

pub(crate) fn apply_theme_mode(ui: &DarkMatterLinux, mode: &str) {
    let mode = normalize_theme_mode(mode);
    ui.set_light_theme(mode == "light");
    ui.set_retro_mode(mode == "retro");
    ui.set_terminal_mode(mode == "terminal");
    ui.set_crayon_mode(mode == "crayon");
    ui.set_synthwave_mode(mode == "synthwave");
    ui.set_chalkboard_mode(mode == "chalkboard");
}

pub(crate) fn locale_display(code: &str) -> &'static str {
    match normalize_locale(code) {
        "it" => "Italiano",
        "de" => "Deutsch",
        "ja" => "日本語",
        _ => "English",
    }
}

pub(crate) fn apply_locale(locale: &str) {
    let code = normalize_locale(locale);
    if let Err(e) = slint::select_bundled_translation(code) {
        eprintln!("[i18n] select_bundled_translation({code}): {e}");
        let _ = slint::select_bundled_translation("en");
    }
}
