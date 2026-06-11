use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use marmot_app::{
    npub_for_account_id, AppGroupMemberRecord, AppGroupRecord, AppMessageRecord,
    MediaAttachmentReference, MediaLocator, UserDirectoryRecord, UserProfileMetadata,
};
use nostr::nips::nip19::ToBech32;
use nostr::Keys;
use slint::{Color, Model, ModelRc, SharedString, VecModel, Weak};
use tokio::task::JoinHandle;

mod backend;
mod blossom;
mod media_cache;
mod observability;
mod settings;
mod vault;

use backend::Backend;
use settings::Settings;
use vault::Vault;

slint::include_modules!();

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
    // When `Some`, this is a media upload + send. The optimistic bubble renders
    // the attachment chip/preview while we wait for the encrypt+blossom+publish
    // round-trip to resolve.
    media: Option<PendingMedia>,
}

#[derive(Clone)]
struct PendingMedia {
    file_name: String,
    media_type: String,
    size_bytes: u64,
    is_image: bool,
    // Local pixels for instant image preview while the upload is in flight.
    // None for non-image attachments.
    local_preview: Option<PicturePixels>,
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
    /// in-flight op per target; cleared when the kind-1010 send resolves.
    edits: HashMap<(String, String), String>,
}

impl PendingState {
    fn add_send(&mut self, group_hex: &str, send: PendingSend) {
        self.sends.entry(group_hex.to_string()).or_default().push(send);
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
        self.sends.get(group_hex)
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

fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(v))
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
    let base = code.split('.').next().unwrap_or(code).split('_').next().unwrap_or(code);
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
    apply_theme_mode(&ui, &theme_mode);
    ui.set_accent_color(accent_color_idx(accent_color));
    ui.set_outgoing_on_right(initial_settings.outgoing_on_right);
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
    let backend_cell: Arc<Mutex<Option<Backend>>> = Arc::new(Mutex::new(None));
    // The unlocked secret vault for this session. Held behind `Arc<Mutex>` so a
    // clone can be moved into the boot worker thread (and into marmot's secret
    // store) while the UI thread keeps its own handle. `None` until the user
    // unlocks or creates a vault on the login screen.
    let vault_cell: Rc<RefCell<Option<Arc<Mutex<Vault>>>>> = Rc::new(RefCell::new(None));
    let group_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let archived_group_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    // Optimistic-render overlay: pending sends + pending reactions. Lives
    // behind `Arc<Mutex<…>>` because async send/react callbacks fire on the
    // tokio worker thread and need to mutate it before hopping back to the
    // Slint event loop via `invoke_from_event_loop` (which requires Send).
    let pending_state: Arc<Mutex<PendingState>> = Arc::new(Mutex::new(PendingState::default()));
    // Currently-active per-chat message watcher. Aborted and replaced when the
    // user switches chats so we never leak background tasks.
    let active_message_watcher: Rc<RefCell<Option<JoinHandle<()>>>> = Rc::new(RefCell::new(None));

    // ─── Login gate ────────────────────────────────────────────────────
    // Holds the freshly generated nsec until the user confirms they've saved it.
    let pending_generated: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    // Boot the backend from an nsec and populate the chat models. Errors are
    // surfaced on the UI's backend-error property; the UI stays logged-in
    // either way so the user can still navigate.
    let boot_backend: Rc<dyn Fn(String, Arc<Mutex<Vault>>)> = {
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        let archived_group_ids = archived_group_ids.clone();
        let vault_cell = vault_cell.clone();
        Rc::new(move |nsec: String, vault: Arc<Mutex<Vault>>| {
            let Some(ui) = weak.upgrade() else { return };
            // Keep the unlocked vault for the rest of the session.
            *vault_cell.borrow_mut() = Some(vault.clone());
            ui.set_backend_ready(false);
            ui.set_backend_error(s(""));
            ui.set_booting(true);
            ui.set_booting_status(s("Opening account home…"));

            // Hand the boot off to a worker thread so the Slint event loop
            // keeps rendering the splash screen. Send the result back via
            // invoke_from_event_loop. Capture only Send data — model handles
            // are `Rc`-based (!Send), so we look them up off the UI handle
            // inside the invoke closure instead.
            let weak_for_worker = weak.clone();
            let backend_cell = backend_cell.clone();
            let group_ids = group_ids.clone();
            let archived_group_ids = archived_group_ids.clone();
            std::thread::spawn(move || {
                let relays = backend::load_relays();
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
                let on_synced = move |sync_result: anyhow::Result<()>| {
                    let _ = slint::invoke_from_event_loop(move || {
                        sync_done_for_sync.store(true, std::sync::atomic::Ordering::Relaxed);
                        let Some(ui) = weak_for_sync.upgrade() else { return };
                        if let Err(e) = sync_result {
                            eprintln!("[backend] background sync failed: {e:#}");
                            ui.set_backend_error(format!("sync: {e:#}").into());
                            return;
                        }
                        let guard = backend_cell_for_sync.lock().unwrap();
                        let Some(b) = guard.as_ref() else { return };
                        // The directory sync just finished — re-pull every
                        // cached name/picture so changes made while we were
                        // offline converge (async; next rebuilds pick them up).
                        b.refresh_all_profiles_async();
                        let chats = ui.get_chats();
                        let chats_messages = ui.get_chats_messages();
                        merge_chat_list_rows(b, &chats, &chats_messages, &group_ids_for_sync);
                        set_rail_badges(&ui, &chats);
                        refresh_contacts(b, &ui.get_contacts(), &Settings::load().nicknames);
                        refresh_archived(b, &ui.get_archived_chats(), &archived_for_sync);
                        spawn_chat_list_avatar_fetches(&ui, b);
                        populate_profile(b, &ui);
                        refresh_kp_local(b, &ui);
                        refresh_network_post_boot(b, &ui);
                        drop(guard);
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
                                let guard = backend_cell2.lock().unwrap();
                                let Some(b) = guard.as_ref() else { return };
                                let chats = ui.get_chats();
                                merge_chat_list_rows(
                                    b,
                                    &chats,
                                    &ui.get_chats_messages(),
                                    &group_ids2,
                                );
                                set_rail_badges(&ui, &chats);
                            },
                        );
                    });
                };
                let result = Backend::boot(&nsec, relays, secret_store, on_synced);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak_for_worker.upgrade() else { return };
                    match result {
                        Ok(b) => {
                            let t_ui = std::time::Instant::now();
                            let chats = ui.get_chats();
                            let chats_messages = ui.get_chats_messages();
                            let contacts = ui.get_contacts();
                            let archived = ui.get_archived_chats();
                            refresh_chats(&b, &chats, &chats_messages, &group_ids);
                            eprintln!("[boot-timing] ui refresh_chats done at {:?}", t_ui.elapsed());
                            set_rail_badges(&ui, &chats);
                            // This closure crosses a thread boundary (must be
                            // Send), so it can't capture the Rc settings cell —
                            // re-read the nickname map from disk instead.
                            refresh_contacts(&b, &contacts, &Settings::load().nicknames);
                            refresh_archived(&b, &archived, &archived_group_ids);
                            spawn_chat_list_avatar_fetches(&ui, &b);
                            eprintln!("[boot-timing] ui contacts/archived done at {:?}", t_ui.elapsed());
                            if let Some(hex) = group_ids.lock().unwrap().first().cloned() {
                                push_group_members_to_ui(&ui, &b, &hex);
                                let msgs = b.messages(&hex, Some(msg_window_for(&hex))).unwrap_or_default();
                                ui.set_messages_has_older(msgs.len() >= MESSAGE_WINDOW);
                                spawn_message_avatar_fetches(&ui, &b, &msgs);
                            }
                            eprintln!("[boot-timing] ui first-chat done at {:?}", t_ui.elapsed());
                            populate_profile(&b, &ui);
                            refresh_kp_local(&b, &ui);
                            refresh_network_post_boot(&b, &ui);
                            eprintln!("[boot-timing] ui post-boot done at {:?}", t_ui.elapsed());
                            // Security & privacy settings live in marmot storage;
                            // surface their current state once the backend is up.
                            ui.set_telemetry_enabled(b.telemetry_enabled());
                            ui.set_audit_enabled(b.audit_logs_enabled());
                            install_chat_watcher(&b, ui.as_weak(), group_ids.clone(), backend_cell.clone());
                            *backend_cell.lock().unwrap() = Some(b);
                            ui.set_backend_ready(true);
                            ui.set_booting(false);
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
                                        let guard = backend_cell.lock().unwrap();
                                        let Some(b) = guard.as_ref() else { return };
                                        let chats = ui.get_chats();
                                        merge_chat_list_rows(
                                            b,
                                            &chats,
                                            &ui.get_chats_messages(),
                                            &group_ids,
                                        );
                                        set_rail_badges(&ui, &chats);
                                        refresh_contacts(
                                            b,
                                            &ui.get_contacts(),
                                            &Settings::load().nicknames,
                                        );
                                        spawn_chat_list_avatar_fetches(&ui, b);
                                        populate_profile(b, &ui);
                                    },
                                );
                            }
                        }
                        Err(e) => {
                            eprintln!("[backend] boot failed: {e:#}");
                            ui.set_backend_error(format!("backend: {e:#}").into());
                            ui.set_booting(false);
                        }
                    }
                });
            });
        })
    };

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
            ui.set_login_busy(true);
            let result = (|| -> Result<(String, String, Arc<Mutex<Vault>>), String> {
                validate_new_password(&password, confirm.as_str())?;
                let keys = Keys::parse(&trimmed)
                    .map_err(|_| "That doesn't look like a valid nsec.".to_string())?;
                let npub = keys.public_key().to_bech32().map_err(|e| e.to_string())?;
                let nsec = keys.secret_key().to_bech32().map_err(|e| e.to_string())?;
                let mut v = Vault::create(&password).map_err(|e| format!("create vault: {e}"))?;
                v.set(vault::NSEC_KEY, &nsec)
                    .map_err(|e| format!("seal nsec: {e}"))?;
                Ok((npub, nsec, Arc::new(Mutex::new(v))))
            })();
            ui.set_login_busy(false);
            match result {
                Ok((npub, nsec, vault)) => {
                    ui.set_login_error(s(""));
                    ui.set_my_npub(npub.into());
                    ui.set_login_nsec_input(s(""));
                    ui.set_password_input(s(""));
                    ui.set_password_confirm(s(""));
                    ui.set_logged_in(true);
                    boot(nsec, vault);
                }
                Err(err) => {
                    ui.set_login_error(err.into());
                }
            }
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
            let result = (|| -> Result<(String, String, Arc<Mutex<Vault>>), String> {
                let v = Vault::open(&password).map_err(|e| match e {
                    vault::VaultError::WrongPassword => "Wrong password.".to_string(),
                    other => format!("{other}"),
                })?;
                let nsec = v
                    .nsec()
                    .ok_or_else(|| "Vault has no key. Reset and re-enter your nsec.".to_string())?;
                let keys =
                    Keys::parse(&nsec).map_err(|_| "Stored key is invalid.".to_string())?;
                let npub = keys.public_key().to_bech32().map_err(|e| e.to_string())?;
                Ok((npub, nsec, Arc::new(Mutex::new(v))))
            })();
            ui.set_login_busy(false);
            match result {
                Ok((npub, nsec, vault)) => {
                    ui.set_login_error(s(""));
                    ui.set_password_input(s(""));
                    ui.set_my_npub(npub.into());
                    ui.set_logged_in(true);
                    boot(nsec, vault);
                }
                Err(err) => {
                    ui.set_login_error(err.into());
                }
            }
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
            ui.set_password_input(s(""));
            ui.set_password_confirm(s(""));
            ui.set_login_error(s(""));
            ui.set_login_mode(0);
        }
    });

    ui.on_generate_key_requested({
        let weak = ui.as_weak();
        let pending = pending_generated.clone();
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
            *pending.borrow_mut() = Some(nsec.clone());
            ui.set_generated_nsec(nsec.into());
            ui.set_generated_npub(npub.into());
            ui.set_login_error(s(""));
            ui.set_login_mode(2);
        }
    });

    ui.on_confirm_saved_key({
        let weak = ui.as_weak();
        let pending = pending_generated.clone();
        let boot = boot_backend.clone();
        move |password, confirm| {
            eprintln!("[login] confirm_saved_key fired");
            let Some(ui) = weak.upgrade() else { return };
            let Some(nsec) = pending.borrow().clone() else {
                eprintln!("[login] no pending generated key");
                ui.set_login_error(s("No generated key to save. Try again."));
                ui.set_login_mode(0);
                return;
            };
            let password = password.to_string();
            ui.set_login_busy(true);
            let result = (|| -> Result<(String, Arc<Mutex<Vault>>), String> {
                validate_new_password(&password, confirm.as_str())?;
                let keys = Keys::parse(&nsec).map_err(|e| format!("parse: {e}"))?;
                let npub = keys
                    .public_key()
                    .to_bech32()
                    .map_err(|e| format!("npub encode: {e}"))?;
                let mut v = Vault::create(&password).map_err(|e| format!("create vault: {e}"))?;
                v.set(vault::NSEC_KEY, &nsec)
                    .map_err(|e| format!("seal nsec: {e}"))?;
                Ok((npub, Arc::new(Mutex::new(v))))
            })();
            match result {
                Ok((npub, vault)) => {
                    eprintln!("[login] sealed nsec into vault, logging in as {npub}");
                    *pending.borrow_mut() = None;
                    ui.set_login_busy(false);
                    ui.set_login_error(s(""));
                    ui.set_my_npub(npub.into());
                    ui.set_generated_nsec(s(""));
                    ui.set_generated_npub(s(""));
                    ui.set_password_input(s(""));
                    ui.set_password_confirm(s(""));
                    ui.set_logged_in(true);
                    boot(nsec, vault);
                }
                Err(err) => {
                    eprintln!("[login] save failed: {err}");
                    ui.set_login_busy(false);
                    ui.set_login_error(err.into());
                }
            }
        }
    });

    ui.on_copy_nsec({
        let weak = ui.as_weak();
        move |nsec| {
            if let Err(e) = copy_to_clipboard(nsec.as_str()) {
                eprintln!("[clipboard] copy nsec failed: {e}");
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_profile_status(s("nsec copied"));
            }
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

    ui.on_debug_refresh_clicked({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let snap = backend_cell
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|b| b.debug_snapshot()))
                .unwrap_or_else(|| "(backend not booted)".to_string());
            ui.set_debug_dump(snap.into());
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
            if let Err(e) = copy_to_clipboard(text.as_str()) {
                eprintln!("[clipboard] copy debug dump failed: {e}");
            }
        }
    });

    // ─── Security & privacy toggles ────────────────────────────────────
    ui.on_telemetry_toggled({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        move |on| {
            let Some(ui) = weak.upgrade() else { return };
            let result = backend_cell
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|b| b.set_telemetry_enabled(on)));
            match result {
                Some(Ok(())) => {}
                Some(Err(e)) => {
                    eprintln!("[settings] set telemetry failed: {e}");
                    ui.set_telemetry_enabled(!on);
                }
                None => ui.set_telemetry_enabled(!on),
            }
        }
    });

    ui.on_audit_toggled({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        move |on| {
            let Some(ui) = weak.upgrade() else { return };
            let result = backend_cell
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|b| b.set_audit_logs_enabled(on)));
            match result {
                Some(Ok(())) => {
                    ui.set_audit_status(
                        if on {
                            "Audit logging enabled. Restart to begin recording; \
                             logs upload automatically after that."
                        } else {
                            "Audit logging disabled. Takes effect after restart."
                        }
                        .into(),
                    );
                }
                Some(Err(e)) => {
                    eprintln!("[settings] set audit logs failed: {e}");
                    ui.set_audit_enabled(!on);
                }
                None => ui.set_audit_enabled(!on),
            }
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
            let Some(vault) = vault_cell.borrow().clone() else {
                return;
            };
            let Some(nsec) = vault.lock().unwrap().nsec() else {
                return;
            };
            // `boot` re-reads `load_relays()` (already saved below), spawns a
            // fresh runtime, and on success replaces backend_cell + re-pushes
            // the live connection counts via refresh_network_post_boot.
            boot(nsec, vault);
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
                ui.set_network_status("Already in the list.".into());
                return;
            }
            list.push(trimmed);
            if let Err(e) = backend::save_relays(&list) {
                ui.set_network_status(format!("Save failed: {e}").into());
                return;
            }
            push_network_relays(&ui, &list);
            ui.set_network_status("Saved.".into());
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
                ui.set_network_status(format!("Save failed: {e}").into());
                return;
            }
            push_network_relays(&ui, &list);
            ui.set_network_status("Removed.".into());
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
                let snapshot = {
                    let guard = backend_cell.lock().unwrap();
                    guard.as_ref().map(|b| b.relay_health())
                };
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    match snapshot {
                        Some((connected, total)) => {
                            ui.set_network_connected(connected as i32);
                            ui.set_network_total(total as i32);
                            // We just polled the relay pool — that's a real sync.
                            ui.set_sync_secs(0);
                        }
                        None => ui.set_network_status("Backend not ready yet.".into()),
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
            ui.set_network_status("Republishing…".into());
            let weak = weak.clone();
            let backend_cell = backend_cell.clone();
            std::thread::spawn(move || {
                let result = {
                    let guard = backend_cell.lock().unwrap();
                    match guard.as_ref() {
                        None => Err("Backend not ready.".to_string()),
                        Some(b) => b.republish_relay_lists().map_err(|e| format!("{e:#}")),
                    }
                };
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    match result {
                        Ok(n) => ui.set_network_status(
                            format!("Republished to {n} relay{}.", if n == 1 { "" } else { "s" })
                                .into(),
                        ),
                        Err(e) => ui.set_network_status(format!("Republish failed: {e}").into()),
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
            let backend_cell = backend_cell.clone();
            std::thread::spawn(move || {
                let result: Result<String, String> = {
                    let guard = backend_cell.lock().unwrap();
                    match guard.as_ref() {
                        None => Err("backend not ready".to_string()),
                        Some(b) => match op_kind {
                            // NOTE: the SDK returns the key-package size in bytes,
                            // not a relay-ack count — so we don't surface the number
                            // (it was being shown as a nonsensical "N relay acks").
                            "publish" => b
                                .publish_key_package()
                                .map(|_| "published · your key package is live".to_string())
                                .map_err(|e| format!("publish failed: {e:#}")),
                            "rotate" => b
                                .rotate_key_package()
                                .map(|_| "rotated · published a fresh key package".to_string())
                                .map_err(|e| format!("rotate failed: {e:#}")),
                            "refresh" => b
                                .key_packages_fetch()
                                .map(|recs| format!("fetched · {} record{}", recs.len(), if recs.len() == 1 { "" } else { "s" }))
                                .map_err(|e| format!("fetch failed: {e:#}")),
                            _ => Err(format!("unknown op: {op_kind}")),
                        },
                    }
                };
                let backend_cell = backend_cell.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    ui.set_kp_busy(false);
                    match result {
                        Ok(status) => ui.set_kp_status(status.into()),
                        Err(e) => ui.set_kp_status(e.into()),
                    }
                    // Refresh from local state regardless of op outcome; for
                    // "refresh" we additionally surface the relay snapshot.
                    let guard = backend_cell.lock().unwrap();
                    if let Some(b) = guard.as_ref() {
                        if op_kind == "refresh" {
                            if let Ok(recs) = b.key_packages_fetch() {
                                let rows: Vec<KeyPackageInfo> = recs.iter().map(kp_to_ui).collect();
                                ui.set_key_packages(ModelRc::new(VecModel::from(rows)));
                            } else {
                                refresh_kp_local(b, &ui);
                            }
                        } else {
                            refresh_kp_local(b, &ui);
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
            match copy_to_clipboard(text.as_str()) {
                Ok(()) => ui.set_profile_status(s("npub copied")),
                Err(e) => {
                    eprintln!("[clipboard] copy failed: {e}");
                    ui.set_profile_status(format!("clipboard error: {e}").into());
                }
            }
        }
    });


    // After any selection mutation, refresh the breadcrumb so the title bar matches state.
    let refresh_breadcrumb = {
        let weak = ui.as_weak();
        let chats = chats.clone();
        let contacts = contacts.clone();
        let archived = archived.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_breadcrumb(breadcrumb(
                ui.get_active_page(),
                &chats,
                &contacts,
                &archived,
                ui.get_active_chat(),
                ui.get_active_contact(),
                ui.get_active_archived(),
            ));
        }
    };
    refresh_breadcrumb();

    let go_to_page = {
        let weak = ui.as_weak();
        let refresh = refresh_breadcrumb.clone();
        move |page: Page| {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_active_page(page as i32);
            refresh();
        }
    };

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
            let guard = backend_cell.lock().unwrap();
            let Some(b) = guard.as_ref() else {
                ui.set_new_chat_status(s("Backend not ready."));
                return;
            };
            // Skip the creator's own npub if it leaked into the input —
            // marmot rejects self-invites.
            let me_npub = npub_for_account_id(&b.account().account_id_hex).ok();
            let members: Vec<String> = members
                .into_iter()
                .filter(|m| me_npub.as_deref().map(|n| !m.eq_ignore_ascii_case(n)).unwrap_or(true))
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
            match b.create_group(&group_name, &members) {
                Ok(group_id) => {
                    let group_hex = hex::encode(group_id.as_slice());
                    let chats = ui.get_chats();
                    let chats_messages = ui.get_chats_messages();
                    refresh_chats(b, &chats, &chats_messages, &group_ids);
                    set_rail_badges(&ui, &chats);
                    spawn_chat_list_avatar_fetches(&ui, b);
                    // Select the freshly-created chat. The runtime appends it
                    // to the visible-chats projection synchronously after
                    // create_group resolves, so it should be present.
                    let pos = group_ids
                        .lock()
                        .unwrap()
                        .iter()
                        .position(|g| g.eq_ignore_ascii_case(&group_hex));
                    drop(guard);
                    if let Some(pos) = pos {
                        ui.set_active_chat(pos as i32);
                        ui.invoke_chat_selected(pos as i32);
                    }
                    ui.set_new_chat_busy(false);
                    ui.set_new_chat_name(s(""));
                    ui.set_new_chat_members(s(""));
                    ui.set_new_chat_status(s(""));
                    ui.set_show_new_chat(false);
                }
                Err(e) => {
                    eprintln!("[create-group] {e:#}");
                    ui.set_new_chat_busy(false);
                    ui.set_new_chat_status(format!("Failed: {e:#}").into());
                }
            }
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
        let contacts = contacts.clone();
        let refresh = refresh_breadcrumb.clone();
        let settings_cell = settings_cell.clone();
        move |input| {
            let Some(ui) = weak.upgrade() else { return };
            let input = input.trim().to_string();
            if input.is_empty() {
                ui.set_add_contact_status(s("Paste an npub or hex pubkey."));
                return;
            }
            let guard = backend_cell.lock().unwrap();
            let Some(b) = guard.as_ref() else {
                ui.set_add_contact_status(s("Backend not ready."));
                return;
            };
            ui.set_add_contact_busy(true);
            ui.set_add_contact_status(s(""));
            match b.add_contact(&input) {
                Ok(account_id_hex) => {
                    refresh_contacts(b, &contacts, &settings_cell.borrow().nicknames);
                    // Select the freshly-added row so the detail pane shows it.
                    if let Ok(npub) = npub_for_account_id(&account_id_hex) {
                        if let Some(pos) = contacts
                            .iter()
                            .position(|c| c.npub_full.as_str().eq_ignore_ascii_case(&npub))
                        {
                            ui.set_active_contact(pos as i32);
                        }
                    }
                    ui.set_add_contact_busy(false);
                    ui.set_add_contact_input(s(""));
                    ui.set_add_contact_status(s(""));
                    ui.set_show_add_contact(false);
                    refresh();
                }
                Err(e) => {
                    eprintln!("[add-contact] {e:#}");
                    ui.set_add_contact_busy(false);
                    ui.set_add_contact_status(format!("Failed: {e:#}").into());
                }
            }
        }
    });
    // "Add contact" from the peer-profile modal — same flow as the add-contact
    // modal, but feedback stays inside the profile modal (badge flip / status).
    ui.on_peer_profile_add_contact({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let contacts = contacts.clone();
        let refresh = refresh_breadcrumb.clone();
        let settings_cell = settings_cell.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let npub = ui.get_peer_profile_npub().to_string();
            if npub.is_empty() {
                return;
            }
            let guard = backend_cell.lock().unwrap();
            let Some(b) = guard.as_ref() else { return };
            ui.set_peer_profile_adding(true);
            ui.set_peer_profile_status(s(""));
            match b.add_contact(&npub) {
                Ok(_) => {
                    refresh_contacts(b, &contacts, &settings_cell.borrow().nicknames);
                    ui.set_peer_profile_adding(false);
                    ui.set_peer_profile_is_contact(true);
                    refresh();
                }
                Err(e) => {
                    eprintln!("[profile-add-contact] {e:#}");
                    ui.set_peer_profile_adding(false);
                    ui.set_peer_profile_status(format!("Failed: {e:#}").into());
                }
            }
        }
    });
    ui.on_contact_nickname_requested({
        let weak = ui.as_weak();
        let contacts = contacts.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(row) = contacts.row_data(ui.get_active_contact() as usize) else { return };
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
            let Some(mut row) = contacts.row_data(idx) else { return };
            let nick = nick.trim().to_string();
            {
                let mut st = settings_cell.borrow_mut();
                if nick.is_empty() {
                    st.nicknames.remove(row.account_id.as_str());
                } else {
                    st.nicknames.insert(row.account_id.to_string(), nick.clone());
                }
                st.save();
            }
            // Patch the one row in place — no relay round-trip involved.
            row.name = if nick.is_empty() { row.real_name.clone() } else { nick.clone().into() };
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
            ui.set_add_member_busy(true);
            ui.set_add_member_status(s(""));
            let guard = backend_cell.lock().unwrap();
            let Some(b) = guard.as_ref() else {
                ui.set_add_member_busy(false);
                ui.set_add_member_status(s("Backend not ready."));
                return;
            };
            match b.invite_members(&group_hex, &[npub.clone()]) {
                Ok(_) => {
                    push_group_members_to_ui(&ui, b, &group_hex);
                    ui.set_add_member_busy(false);
                    ui.set_add_member_draft(s(""));
                    ui.set_add_member_status(s("Invited."));
                }
                Err(e) => {
                    eprintln!("[invite] {e:#}");
                    ui.set_add_member_busy(false);
                    ui.set_add_member_status(format!("Failed: {e:#}").into());
                }
            }
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
            let guard = backend_cell.lock().unwrap();
            let Some(b) = guard.as_ref() else {
                ui.set_group_settings_status(s("Backend not ready."));
                return;
            };
            match b.promote_admin(&group_hex, &member_id) {
                Ok(_) => {
                    push_group_members_to_ui(&ui, b, &group_hex);
                    ui.set_group_settings_status(s("Admin added."));
                }
                Err(e) => {
                    eprintln!("[promote] {e:#}");
                    ui.set_group_settings_status(format!("Failed: {e:#}").into());
                }
            }
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
            let guard = backend_cell.lock().unwrap();
            let Some(b) = guard.as_ref() else {
                ui.set_group_settings_status(s("Backend not ready."));
                return;
            };
            match b.demote_admin(&group_hex, &member_id) {
                Ok(_) => {
                    push_group_members_to_ui(&ui, b, &group_hex);
                    ui.set_group_settings_status(s("Admin removed."));
                }
                Err(e) => {
                    eprintln!("[demote] {e:#}");
                    ui.set_group_settings_status(format!("Failed: {e:#}").into());
                }
            }
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
            let guard = backend_cell.lock().unwrap();
            let Some(b) = guard.as_ref() else {
                ui.set_group_settings_status(s("Backend not ready."));
                return;
            };
            match b.self_demote_admin(&group_hex) {
                Ok(_) => {
                    push_group_members_to_ui(&ui, b, &group_hex);
                    ui.set_group_settings_status(s("You stepped down."));
                }
                Err(e) => {
                    eprintln!("[self-demote] {e:#}");
                    ui.set_group_settings_status(format!("Failed: {e:#}").into());
                }
            }
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
            ui.set_group_rename_busy(true);
            ui.set_group_settings_status(s(""));
            let guard = backend_cell.lock().unwrap();
            let Some(b) = guard.as_ref() else {
                ui.set_group_rename_busy(false);
                ui.set_group_settings_status(s("Backend not ready."));
                return;
            };
            match b.rename_group(&group_hex, &name) {
                Ok(_) => {
                    let chats = ui.get_chats();
                    let chats_messages = ui.get_chats_messages();
                    refresh_chats(b, &chats, &chats_messages, &group_ids);
                    push_group_members_to_ui(&ui, b, &group_hex);
                    ui.set_group_rename_busy(false);
                    ui.set_group_settings_status(s("Renamed."));
                }
                Err(e) => {
                    eprintln!("[rename] {e:#}");
                    ui.set_group_rename_busy(false);
                    ui.set_group_settings_status(format!("Failed: {e:#}").into());
                }
            }
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
                    let Some(ui) = weak_done.upgrade() else { return };
                    ui.set_group_image_busy(false);
                    match result {
                        Ok(_) => {
                            ui.set_group_settings_status(s("image removed"));
                            if let Some(b) = backend_cell_done.lock().unwrap().as_ref() {
                                let chats = ui.get_chats();
                                let chats_messages = ui.get_chats_messages();
                                refresh_chats(b, &chats, &chats_messages, &group_ids);
                                push_group_members_to_ui(&ui, b, &group_hex_done);
                            }
                        }
                        Err(e) => {
                            eprintln!("[group-image] clear failed: {e:#}");
                            ui.set_group_settings_status(format!("Failed: {e:#}").into());
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
                        let Some(ui) = weak_done.upgrade() else { return };
                        ui.set_group_image_busy(false);
                        match result {
                            Ok(_) => {
                                ui.set_group_settings_status(s("group image updated"));
                                if let Some(backend) = backend_cell_done.lock().unwrap().as_ref() {
                                    let chats = ui.get_chats();
                                    let chats_messages = ui.get_chats_messages();
                                    refresh_chats(backend, &chats, &chats_messages, &group_ids_done);
                                    push_group_members_to_ui(&ui, backend, &group_hex_done);
                                }
                            }
                            Err(e) => {
                                eprintln!("[group-image] upload failed: {e:#}");
                                ui.set_group_settings_status(format!("upload failed: {e:#}").into());
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
        let chats_messages = chats_messages.clone();
        let active_watcher = active_message_watcher.clone();
        let pending_state = pending_state.clone();
        move |idx| {
            if let Some(ui) = weak.upgrade() {
                ui.set_active_chat(idx);
                // Reply targets are per-chat; switching threads should not
                // leak a stale "Replying to …" chip across conversations.
                ui.set_reply_target_id(s(""));
                ui.set_reply_target_author(s(""));
                ui.set_reply_target_preview(s(""));
                refresh();
                let guard = backend_cell.lock().unwrap();
                if let Some(backend) = guard.as_ref() {
                    let group_hex = group_ids.lock().unwrap().get(idx as usize).cloned();
                    if let Some(group_hex) = group_hex {
                        let t_switch = std::time::Instant::now();
                        // Re-entering a chat always starts from the default
                        // window — expanded history is per-visit.
                        msg_window_reset(&group_hex);
                        // Snapshot first (merged with any pending overlay for
                        // this chat — chat switching shouldn't drop pending
                        // bubbles).
                        let my_id = backend.account().account_id_hex.clone();
                        let overlay = pending_state.lock().unwrap();
                        let msgs = rebuild_chat_messages(
                            backend,
                            &overlay,
                            &chats_messages,
                            idx as usize,
                            &group_hex,
                        );
                        drop(overlay);
                        ui.set_messages_has_older(msgs.len() >= MESSAGE_WINDOW);
                        spawn_message_avatar_fetches(&ui, backend, &msgs);
                        eprintln!(
                            "[switch-timing] chat {idx}: {} records rebuilt in {:?}",
                            msgs.len(),
                            t_switch.elapsed()
                        );
                        ui.set_show_chat_members(false);
                        push_group_members_to_ui(&ui, backend, &group_hex);
                        // Opening a chat should land you at the most recent
                        // message, not the top of the history.
                        ui.set_messages_scroll_tick(ui.get_messages_scroll_tick() + 1);
                        // Then attach a live watcher for new arrivals. Abort
                        // any previous one so we don't pile them up.
                        if let Some(prev) = active_watcher.borrow_mut().take() {
                            prev.abort();
                        }
                        let handle = install_message_watcher(
                            backend,
                            ui.as_weak(),
                            backend_cell.clone(),
                            pending_state.clone(),
                            group_hex,
                            idx as usize,
                            my_id,
                        );
                        *active_watcher.borrow_mut() = Some(handle);
                    }
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
        let chats_messages = chats_messages.clone();
        let pending_state = pending_state.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let idx = ui.get_active_chat() as usize;
            let Some(group_hex) = group_ids.lock().unwrap().get(idx).cloned() else {
                return;
            };
            let guard = backend_cell.lock().unwrap();
            let Some(backend) = guard.as_ref() else { return };
            let new_window = msg_window_expand(&group_hex);
            let overlay = pending_state.lock().unwrap();
            let msgs =
                rebuild_chat_messages(backend, &overlay, &chats_messages, idx, &group_hex);
            drop(overlay);
            // Fewer records than asked for → the full history is loaded.
            ui.set_messages_has_older(msgs.len() >= new_window);
            spawn_message_avatar_fetches(&ui, backend, &msgs);
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
                let guard = backend_cell.lock().unwrap();
                if let Some(backend) = guard.as_ref() {
                    let hex = archived_group_ids
                        .lock()
                        .unwrap()
                        .get(idx as usize)
                        .cloned();
                    if let Some(group_hex) = hex {
                        push_group_members_to_ui(&ui, backend, &group_hex);
                    }
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
            let guard = backend_cell.lock().unwrap();
            let Some(b) = guard.as_ref() else { return };
            let chats = ui.get_chats();
            let chats_messages = ui.get_chats_messages();
            let archived = ui.get_archived_chats();
            refresh_chats(b, &chats, &chats_messages, &group_ids);
            set_rail_badges(&ui, &chats);
            refresh_archived(b, &archived, &archived_group_ids);
            spawn_chat_list_avatar_fetches(&ui, b);
            // Clamp active-chat so we don't dangle past the end after a row was removed.
            let len = chats.row_count() as i32;
            if ui.get_active_chat() >= len {
                ui.set_active_chat((len - 1).max(0));
            }
            let alen = archived.row_count() as i32;
            if ui.get_active_archived() >= alen {
                ui.set_active_archived((alen - 1).max(0));
            }
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
            let guard = backend_cell.lock().unwrap();
            let Some(b) = guard.as_ref() else {
                ui.set_backend_error(s("backend not ready"));
                return;
            };
            if let Err(e) = b.accept_group_invite(&group_hex) {
                eprintln!("[accept] {e:#}");
                ui.set_backend_error(format!("accept: {e:#}").into());
                return;
            }
            drop(guard);
            refresh();
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
            let guard = backend_cell.lock().unwrap();
            let Some(b) = guard.as_ref() else {
                ui.set_backend_error(s("backend not ready"));
                return;
            };
            if let Err(e) = b.decline_group_invite(&group_hex) {
                eprintln!("[block] {e:#}");
                ui.set_backend_error(format!("block: {e:#}").into());
                return;
            }
            drop(guard);
            refresh();
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
            let Some(pos) = ids.iter().position(|g| g == &group_hex) else { return };
            let Some(chats_vm) = chats.as_any().downcast_ref::<VecModel<ChatMeta>>() else {
                return;
            };
            let Some(removed_meta) = chats_vm.row_data(pos) else { return };

            // 1. Optimistic UI mutation. Drop the chat row + its parallel
            //    messages model + its id. Append an `ArchivedChat` shaped
            //    from the existing ChatMeta so the archive page reflects it
            //    without waiting on a backend snapshot.
            chats_vm.remove(pos);
            let chats_messages = ui.get_chats_messages();
            if let Some(outer_vm) = chats_messages
                .as_any()
                .downcast_ref::<VecModel<ModelRc<ChatMessage>>>()
            {
                if pos < outer_vm.row_count() {
                    outer_vm.remove(pos);
                }
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
                        ui.set_backend_error(format!("archive: {e:#}").into());
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

            // Resolve real group id via the backend's archived snapshot —
            // ArchivedChat.group_id is rendered as "mls:0x<short>".
            let (group_hex, my_id, meta_from_record) = {
                let guard = backend_cell.lock().unwrap();
                let Some(b) = guard.as_ref() else {
                    ui.set_backend_error(s("backend not ready"));
                    return;
                };
                let Ok(records) = b.archived_chats() else { return };
                let Some(record) = records.get(idx).cloned() else { return };
                let my_id = b.account().account_id_hex.clone();
                let meta = chat_meta_from(&record, None, &my_id, b);
                (record.group_id_hex.clone(), my_id, meta)
            };
            let _ = my_id;

            // 1. Optimistic: pop the archived row, push the chat back into
            //    the chats model.
            let archived_model = ui.get_archived_chats();
            if let Some(vm) = archived_model
                .as_any()
                .downcast_ref::<VecModel<ArchivedChat>>()
            {
                if idx < vm.row_count() {
                    vm.remove(idx);
                }
            }
            if let Some(chats_vm) = ui.get_chats().as_any().downcast_ref::<VecModel<ChatMeta>>() {
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

            // 2. Commit on a worker thread; reconcile with a full refresh on
            //    failure.
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
                        ui.set_backend_error(format!("unarchive: {e:#}").into());
                        refresh_cb();
                    });
                }
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
                    if let Err(e) = copy_to_clipboard(npub.as_str()) {
                        eprintln!("[clipboard] copy npub failed: {e}");
                    }
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
        Rc::new(move |group_hex: String, text: String, temp_id: String, parent_id: Option<String>| {
            let guard = backend_cell.lock().unwrap();
            let Some(backend) = guard.as_ref() else {
                return;
            };
            let weak_cb = weak.clone();
            let group_ids_cb = group_ids.clone();
            let pending_state_cb = pending_state.clone();
            let backend_cell_cb = backend_cell.clone();
            let group_hex_cb = group_hex.clone();
            let temp_id_cb = temp_id.clone();
            let on_done = move |result: anyhow::Result<marmot_app::SendSummary>| {
                // Tokio worker. `ModelRc` is `!Send` — look it up off the UI
                // handle inside the invoke closure.
                let weak = weak_cb.clone();
                let group_ids = group_ids_cb.clone();
                let pending_state = pending_state_cb.clone();
                let backend_cell = backend_cell_cb.clone();
                let group_hex = group_hex_cb.clone();
                let temp_id = temp_id_cb.clone();
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

                            let guard = backend_cell.lock().unwrap();
                            let Some(backend) = guard.as_ref() else { return };
                            let overlay = pending_state.lock().unwrap();
                            let my_id = backend.account().account_id_hex.clone();
                            let my_label = my_avatar_label(backend, &my_id);

                            let confirmed_row: Option<ChatMessage> = real_id
                                .as_deref()
                                .and_then(|id| {
                                    let all = backend
                                        .messages(&group_hex, Some(50))
                                        .unwrap_or_default();
                                    let rec = all
                                        .iter()
                                        .find(|m| m.message_id_hex == id)
                                        .cloned()?;
                                    Some(build_one_message_row(
                                        &rec, &all, &my_id, &my_label, &group_hex, &overlay, backend,
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
                                rebuild_chat_messages(
                                    backend,
                                    &overlay,
                                    &chats_messages,
                                    idx,
                                    &group_hex,
                                );
                            }
                        }
                        Err(e) => {
                            eprintln!("[send] {e:#}");
                            ui.set_backend_error(format!("send: {e:#}").into());
                            // Mark failed in place — the bubble flips to red
                            // without disturbing its neighbours.
                            let mut overlay = pending_state.lock().unwrap();
                            overlay.mark_send_failed(&group_hex, &temp_id);
                            let failed_send = overlay.find_send(&group_hex, &temp_id);
                            drop(overlay);
                            if let Some(failed) = failed_send {
                                let guard = backend_cell.lock().unwrap();
                                let Some(backend) = guard.as_ref() else { return };
                                let my_id = backend.account().account_id_hex.clone();
                                let my_label = my_avatar_label(backend, &my_id);
                                let _ = with_inner_messages(&chats_messages, idx, |vm| {
                                    if let Some(pos) = find_message_row(vm, &temp_id) {
                                        let mut row = pending_chat_message(&failed, &my_id, &my_label);
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
        })
    };

    // ─── Edit dispatch (optimistic, surgical) ─────────────────────────
    //
    // Same shape as `react_op`: stamp the overlay, rewrite ONLY the target
    // bubble's text locally, publish the kind-1010 in the background, then on
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
            let Some(backend) = guard.as_ref() else { return };
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
                    let mut overlay = pending_state.lock().unwrap();
                    if let Err(e) = result {
                        eprintln!("[edit] {e:#}");
                        ui.set_backend_error(format!("edit: {e:#}").into());
                    }
                    overlay.edits.remove(&(group_hex.clone(), target.clone()));
                    let guard = backend_cell.lock().unwrap();
                    let Some(backend) = guard.as_ref() else { return };
                    let ids = group_ids.lock().unwrap();
                    let Some(idx) = ids.iter().position(|g| g == &group_hex) else {
                        return;
                    };
                    let chats_messages = ui.get_chats_messages();
                    refresh_one_message_row(
                        backend,
                        &overlay,
                        &chats_messages,
                        idx,
                        &group_hex,
                        &target,
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
        let dispatch_send = dispatch_send.clone();
        let edit_op = edit_op.clone();
        move |text| {
            let Some(ui) = weak.upgrade() else { return };
            // A send closes the mention picker; the draft is about to clear.
            ui.set_mention_active(false);
            let text = text.trim().to_string();
            if text.is_empty() {
                return;
            }
            // Edit mode: when an edit target is set, this "send" rewrites that
            // message via a kind-1010 instead of posting a new one. Clear the
            // edit state + composer first so the banner drops immediately.
            let editing_id = ui.get_editing_message_id().to_string();
            if !editing_id.is_empty() {
                ui.set_editing_message_id(s(""));
                ui.set_composer_draft(s(""));
                edit_op(editing_id, text);
                return;
            }
            let idx = ui.get_active_chat() as usize;
            let Some(group_hex) = group_ids.lock().unwrap().get(idx).cloned() else {
                return;
            };
            let guard = backend_cell.lock().unwrap();
            let Some(backend) = guard.as_ref() else {
                ui.set_backend_error(s("backend not ready"));
                return;
            };

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
                media: None,
            };
            {
                let mut overlay = pending_state.lock().unwrap();
                overlay.add_send(&group_hex, send.clone());
            }
            let my_id = backend.account().account_id_hex.clone();
            let my_label = my_avatar_label(backend, &my_id);
            let pending_row = pending_chat_message(&send, &my_id, &my_label);
            with_inner_messages(&chats_messages, idx, |vm| push_message_grouped(vm, pending_row));
            ui.set_composer_draft(s(""));
            // Force-scroll to the new bubble. The MessagesArea watches this
            // tick and animates viewport-y to the bottom — so the user sees
            // their message even if they were paged up reading history.
            ui.set_messages_scroll_tick(ui.get_messages_scroll_tick() + 1);
            drop(guard);

            // 2. Dispatch the real send in the background.
            let parent_id = reply_to.as_ref().map(|(id, _, _)| id.clone());
            dispatch_send(group_hex, text, temp_id, parent_id);
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
            let parent_id = send.reply_to.as_ref().map(|(id, _, _)| id.clone());
            dispatch_send(group_hex, send.text, temp_id, parent_id);
        }
    });

    // ─── Attach file ───────────────────────────────────────────────────
    //
    // Composer paperclip → portal file picker → encrypted Blossom upload →
    // kind-9 publish, all in one async flow. Mirrors `send_text_async`'s
    // optimistic-overlay pattern: insert a pending bubble (with the local
    // bytes already decoded as a preview for images), kick off the upload
    // off-UI, then reconcile the pending row when the round-trip resolves.
    //
    // Thread-safety: `ModelRc` is `!Send`, so we never carry it across the
    // tokio boundary — every closure that hops back to the UI re-fetches
    // the model via `ui.get_chats_messages()`.
    ui.on_attach_file({
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
            let guard = backend_cell.lock().unwrap();
            let Some(backend) = guard.as_ref() else { return };
            let tokio_handle = backend.tokio_handle();
            drop(guard);

            let weak_t = weak.clone();
            let backend_cell_t = backend_cell.clone();
            let group_ids_t = group_ids.clone();
            let pending_state_t = pending_state.clone();
            let group_hex_t = group_hex.clone();

            // rfd's xdg-portal backend drives ashpd/zbus. We use the
            // async-std executor flavor of zbus (not tokio) so zbus's own
            // internal connection thread runs its own reactor — no tokio
            // context juggling required. The sync rfd call still goes on a
            // blocking thread so we don't stall a tokio worker.
            tokio_handle.spawn(async move {
                let picked = match tokio::task::spawn_blocking(move || {
                    rfd::FileDialog::new()
                        .set_title("Attach file")
                        .pick_file()
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
                let path = picked;
                let file_name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "attachment".to_string());
                let path_for_read = path.clone();
                let bytes = match tokio::task::spawn_blocking(move || {
                    std::fs::read(&path_for_read)
                })
                .await
                {
                    Ok(Ok(b)) => b,
                    Ok(Err(e)) => {
                        eprintln!("[attach] read {}: {e:#}", path.display());
                        return;
                    }
                    Err(e) => {
                        eprintln!("[attach] read join: {e:#}");
                        return;
                    }
                };
                let media_type = mime_guess::from_path(&path)
                    .first_or_octet_stream()
                    .essence_str()
                    .to_string();
                let size_bytes = bytes.len() as u64;
                let is_image = mime_is_image(&media_type);
                let local_preview: Option<PicturePixels> = if is_image {
                    image::load_from_memory(&bytes).ok().map(|img| {
                        let rgba = img.to_rgba8();
                        let (w, h) = (rgba.width(), rgba.height());
                        PicturePixels {
                            w,
                            h,
                            rgba: rgba.into_raw(),
                        }
                    })
                } else {
                    None
                };

                let weak2 = weak_t.clone();
                let backend_cell2 = backend_cell_t.clone();
                let group_ids2 = group_ids_t.clone();
                let pending_state2 = pending_state_t.clone();
                let group_hex2 = group_hex_t.clone();
                let file_name_u = file_name.clone();
                let media_type_u = media_type.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak2.upgrade() else { return };
                    let chats_messages = ui.get_chats_messages();
                    let ids = group_ids2.lock().unwrap();
                    let Some(idx) = ids.iter().position(|g| g == &group_hex2) else {
                        return;
                    };
                    drop(ids);
                    let guard = backend_cell2.lock().unwrap();
                    let Some(backend) = guard.as_ref() else { return };

                    let temp_id = next_temp_id();
                    let send = PendingSend {
                        temp_id: temp_id.clone(),
                        text: String::new(),
                        failed: false,
                        reply_to: None,
                        media: Some(PendingMedia {
                            file_name: file_name_u.clone(),
                            media_type: media_type_u.clone(),
                            size_bytes,
                            is_image,
                            local_preview: local_preview.clone(),
                        }),
                    };
                    {
                        let mut overlay = pending_state2.lock().unwrap();
                        overlay.add_send(&group_hex2, send.clone());
                    }
                    let my_id = backend.account().account_id_hex.clone();
                    let my_label = my_avatar_label(backend, &my_id);
                    let pending_row = pending_chat_message(&send, &my_id, &my_label);
                    with_inner_messages(&chats_messages, idx, |vm| push_message_grouped(vm, pending_row));
                    ui.set_messages_scroll_tick(ui.get_messages_scroll_tick() + 1);

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
                            let _ = slint::invoke_from_event_loop(move || {
                                let Some(ui) = weak.upgrade() else { return };
                                let chats_messages = ui.get_chats_messages();
                                let ids = group_ids.lock().unwrap();
                                let Some(idx) =
                                    ids.iter().position(|g| g == &group_hex)
                                else {
                                    return;
                                };
                                drop(ids);

                                match result {
                                    Ok(upload) => {
                                        pending_state
                                            .lock()
                                            .unwrap()
                                            .drop_send(&group_hex, &temp_id);
                                        let guard = backend_cell.lock().unwrap();
                                        let Some(backend) = guard.as_ref() else { return };
                                        let real_id = upload
                                            .sent
                                            .as_ref()
                                            .and_then(|s| s.message_ids.first().cloned());
                                        if let (Some(id), Some(p)) =
                                            (real_id.as_ref(), local_preview.as_ref())
                                        {
                                            if is_image {
                                                attachment_image_cache_put(
                                                    id.clone(),
                                                    p.clone(),
                                                );
                                            }
                                        }
                                        let confirmed_row: Option<ChatMessage> = real_id
                                            .as_deref()
                                            .and_then(|id| {
                                                let all = backend
                                                    .messages(&group_hex, Some(50))
                                                    .unwrap_or_default();
                                                let rec = all
                                                    .iter()
                                                    .find(|m| m.message_id_hex == id)
                                                    .cloned()?;
                                                let overlay = pending_state.lock().unwrap();
                                                let my_id =
                                                    backend.account().account_id_hex.clone();
                                                let my_label =
                                                    my_avatar_label(backend, &my_id);
                                                Some(build_one_message_row(
                                                    &rec, &all, &my_id, &my_label,
                                                    &group_hex, &overlay, backend,
                                                ))
                                            });
                                        let swapped = with_inner_messages(
                                            &chats_messages,
                                            idx,
                                            |vm| {
                                                let Some(pos) =
                                                    find_message_row(vm, &temp_id)
                                                else {
                                                    return false;
                                                };
                                                if let Some(mut row) = confirmed_row {
                                                    preserve_grouping_flags(vm, pos, &mut row);
                                                    vm.set_row_data(pos, row);
                                                } else {
                                                    vm.remove(pos);
                                                }
                                                true
                                            },
                                        );
                                        if swapped != Some(true) {
                                            let overlay = pending_state.lock().unwrap();
                                            rebuild_chat_messages(
                                                backend,
                                                &overlay,
                                                &chats_messages,
                                                idx,
                                                &group_hex,
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        eprintln!("[attach] upload: {e:#}");
                                        let mut overlay = pending_state.lock().unwrap();
                                        overlay.mark_send_failed(&group_hex, &temp_id);
                                        let failed =
                                            overlay.find_send(&group_hex, &temp_id);
                                        drop(overlay);
                                        if let Some(failed) = failed {
                                            let guard = backend_cell.lock().unwrap();
                                            let Some(backend) = guard.as_ref() else { return };
                                            let my_id =
                                                backend.account().account_id_hex.clone();
                                            let my_label = my_avatar_label(backend, &my_id);
                                            let _ = with_inner_messages(
                                                &chats_messages,
                                                idx,
                                                |vm| {
                                                    if let Some(pos) =
                                                        find_message_row(vm, &temp_id)
                                                    {
                                                        vm.set_row_data(
                                                            pos,
                                                            pending_chat_message(
                                                                &failed, &my_id, &my_label,
                                                            ),
                                                        );
                                                    }
                                                },
                                            );
                                        }
                                    }
                                }
                            });
                        },
                    );
                });
            });
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
            // lightbox instead of re-downloading.
            if let Some(img) = cached_attachment_image(&mid) {
                ui.set_image_viewer_image(img);
                ui.set_image_viewer_open(true);
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
                attachment_in_flight().lock().ok().map(|mut s| s.remove(&mid));
                return;
            };
            let guard = backend_cell.lock().unwrap();
            let Some(backend) = guard.as_ref() else {
                attachment_in_flight().lock().ok().map(|mut s| s.remove(&mid));
                return;
            };
            let all = backend.messages(&group_hex, Some(msg_window_for(&group_hex))).unwrap_or_default();
            let Some(rec) = all.iter().find(|m| m.message_id_hex == mid).cloned() else {
                attachment_in_flight().lock().ok().map(|mut s| s.remove(&mid));
                return;
            };
            let Some(reference) = parse_media_reference_from_tags(&rec.tags, rec.source_epoch)
            else {
                attachment_in_flight().lock().ok().map(|mut s| s.remove(&mid));
                return;
            };
            let is_image = mime_is_image(&reference.media_type);
            // Unlocked vault for this session (UI thread only). Clones of this
            // Arc ride into the tokio tasks below to seal/unseal the disk cache.
            let vault = vault_cell.borrow().clone();

            let chats_messages = ui.get_chats_messages();
            let overlay = pending_state.lock().unwrap();
            refresh_one_message_row(
                backend,
                &overlay,
                &chats_messages,
                idx,
                &group_hex,
                &mid,
            );
            drop(overlay);
            let tokio_handle = backend.tokio_handle();
            drop(guard);

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
                        attachment_in_flight().lock().ok().map(|mut s| s.remove(&mid));
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
                                            media_cache::put(
                                                v,
                                                &cache_hash,
                                                &dl.plaintext,
                                            );
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
                                            Err(e) => eprintln!(
                                                "[attach] decode {mid}: {e:#}"
                                            ),
                                        }
                                    } else if let Some(path) = &target_path {
                                        if let Err(e) =
                                            std::fs::write(path, &dl.plaintext)
                                        {
                                            eprintln!(
                                                "[attach] write {}: {e:#}",
                                                path.display()
                                            );
                                        }
                                    }
                                }
                                Err(e) => eprintln!("[attach] download {mid}: {e:#}"),
                            }
                            let _ = slint::invoke_from_event_loop(move || {
                                attachment_in_flight()
                                    .lock()
                                    .ok()
                                    .map(|mut s| s.remove(&mid));
                                let Some(ui) = weak.upgrade() else { return };
                                let chats_messages = ui.get_chats_messages();
                                let ids = group_ids.lock().unwrap();
                                let Some(idx) =
                                    ids.iter().position(|g| g == &group_hex)
                                else {
                                    return;
                                };
                                drop(ids);
                                let guard = backend_cell.lock().unwrap();
                                let Some(backend) = guard.as_ref() else { return };
                                let overlay = pending_state.lock().unwrap();
                                refresh_one_message_row(
                                    backend,
                                    &overlay,
                                    &chats_messages,
                                    idx,
                                    &group_hex,
                                    &mid,
                                );
                            });
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
                                    let _ = slint::invoke_from_event_loop(move || {
                                        attachment_image_cache_put(mid.clone(), pixels);
                                        attachment_in_flight()
                                            .lock()
                                            .ok()
                                            .map(|mut s| s.remove(&mid));
                                        let Some(ui) = weak.upgrade() else { return };
                                        let chats_messages = ui.get_chats_messages();
                                        let ids = group_ids.lock().unwrap();
                                        let Some(idx) =
                                            ids.iter().position(|g| g == &group_hex)
                                        else {
                                            return;
                                        };
                                        drop(ids);
                                        let guard = backend_cell.lock().unwrap();
                                        let Some(backend) = guard.as_ref() else { return };
                                        let overlay = pending_state.lock().unwrap();
                                        refresh_one_message_row(
                                            backend,
                                            &overlay,
                                            &chats_messages,
                                            idx,
                                            &group_hex,
                                            &mid,
                                        );
                                    });
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
                            let Some(ui) = weak_clear.upgrade() else { return };
                            let chats_messages = ui.get_chats_messages();
                            let ids = group_ids_clear.lock().unwrap();
                            let Some(idx) =
                                ids.iter().position(|g| g == &group_hex_clear)
                            else {
                                return;
                            };
                            drop(ids);
                            let guard = backend_cell_clear.lock().unwrap();
                            let Some(backend) = guard.as_ref() else { return };
                            let overlay = pending_state_clear.lock().unwrap();
                            refresh_one_message_row(
                                backend,
                                &overlay,
                                &chats_messages,
                                idx,
                                &group_hex_clear,
                                &mid_clear,
                            );
                        }
                    });
                });
            }
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
    // version list (original + each author-authored kind-1010) and open the
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
            let guard = backend_cell.lock().unwrap();
            let Some(backend) = guard.as_ref() else { return };
            let all = backend.messages(&group_hex, Some(msg_window_for(&group_hex))).unwrap_or_default();
            let versions = build_edit_history(&all, message_id.as_str());
            if versions.is_empty() {
                return;
            }
            ui.set_edit_history(ModelRc::new(VecModel::from(versions)));
            ui.set_edit_history_open(true);
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
            let Some(backend) = guard.as_ref() else { return };
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
                    let mut overlay = pending_state.lock().unwrap();
                    if let Err(e) = result {
                        eprintln!("[{label}] {e:#}");
                        ui.set_backend_error(format!("{label}: {e:#}").into());
                    }
                    overlay
                        .reactions
                        .remove(&(group_hex.clone(), target.clone()));
                    let guard = backend_cell.lock().unwrap();
                    let Some(backend) = guard.as_ref() else { return };
                    let ids = group_ids.lock().unwrap();
                    let Some(idx) = ids.iter().position(|g| g == &group_hex) else {
                        return;
                    };
                    let chats_messages = ui.get_chats_messages();
                    refresh_one_message_row(
                        backend,
                        &overlay,
                        &chats_messages,
                        idx,
                        &group_hex,
                        &target,
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
                populate_profile(b, &ui);
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
            let guard = backend_cell.lock().unwrap();
            let Some(backend) = guard.as_ref() else {
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
            let result = backend.save_profile(profile);
            ui.set_profile_busy(false);
            match result {
                Ok(saved) => {
                    apply_profile(&ui, Some(&saved));
                    ui.set_profile_editing(false);
                    ui.set_profile_status(s("profile published"));
                }
                Err(e) => {
                    eprintln!("[profile] save failed: {e:#}");
                    ui.set_profile_status(format!("error: {e:#}").into());
                }
            }
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
                        let Some(ui) = weak_done.upgrade() else { return };
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
                                ui.set_profile_status(format!("upload failed: {e:#}").into());
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
            if let Some(reference) = url.strip_prefix("nostr:") {
                if let Some(hex) = nostr_ref_to_hex(reference) {
                    if let Some(ui) = weak.upgrade() {
                        open_profile_modal(&ui, &backend_cell, &hex);
                        return;
                    }
                }
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

    ui.run()?;
    Ok(())
}

// ─── Profile bridge ────────────────────────────────────────────────────

fn populate_profile(backend: &Backend, ui: &DarkMatterLinux) {
    let picture_url = match backend.load_profile() {
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
    backend_cell: &Arc<Mutex<Option<Backend>>>,
    account_id_hex: &str,
) {
    let guard = backend_cell.lock().unwrap();
    let Some(backend) = guard.as_ref() else { return };
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

    let is_contact = !is_self
        && backend
            .follow_list()
            .map(|l| l.iter().any(|r| r.account_id_hex.eq_ignore_ascii_case(&id)))
            .unwrap_or(false);
    ui.set_peer_profile_is_contact(is_contact);

    if let Some(profile) = backend.cached_profile(&id) {
        ui.set_peer_profile_loading(false);
        apply_peer_profile(ui, backend, &id, &npub_short, Some(&profile));
    } else {
        ui.set_peer_profile_loading(true);
        apply_peer_profile(ui, backend, &id, &npub_short, None);
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let id_done = id.clone();
        let npub_short_done = npub_short.clone();
        backend.fetch_profile_async(&id, move |profile| {
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else { return };
                // Stale guard: the modal may have closed or moved on to a
                // different user while the relay round-trip was in flight.
                if !ui
                    .get_peer_profile_account_id()
                    .as_str()
                    .eq_ignore_ascii_case(&id_done)
                {
                    return;
                }
                ui.set_peer_profile_loading(false);
                let guard = backend_cell.lock().unwrap();
                let Some(backend) = guard.as_ref() else { return };
                match profile {
                    Some(p) => {
                        apply_peer_profile(&ui, backend, &id_done, &npub_short_done, Some(&p))
                    }
                    None => ui.set_peer_profile_not_found(true),
                }
            });
        });
    }
    ui.set_peer_profile_open(true);
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
    let tag = tags
        .iter()
        .find(|t| t.first().map(String::as_str) == Some("imeta"))?;
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

/// Compact byte-size label for attachment chips. KB/MB rounded to one decimal.
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
            if let Some(chat) = vm.row_data(i) {
                if chat.is_chat_request {
                    chats_badge += 1;
                }
            }
        }
    }
    ui.set_rail_badge_chats(chats_badge);
    ui.set_rail_badge_contacts(0);
    ui.set_rail_badge_archive(0);
    ui.set_rail_badge_keys(0);
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
fn replace_message_row(
    outer: &ModelRc<ModelRc<ChatMessage>>,
    idx: usize,
    msgs: Vec<ChatMessage>,
) {
    let inner: ModelRc<ChatMessage> = ModelRc::new(VecModel::from(msgs));
    if let Some(vm) = outer
        .as_any()
        .downcast_ref::<VecModel<ModelRc<ChatMessage>>>()
    {
        if idx < vm.row_count() {
            vm.set_row_data(idx, inner);
        }
    }
}

/// Take a snapshot of chats and (lazily-loaded) messages, push them into the
/// Slint models, and store the parallel group-id list so on_send_message can
/// resolve the active group.
fn refresh_chats(
    backend: &Backend,
    chats: &ModelRc<ChatMeta>,
    chats_messages: &ModelRc<ModelRc<ChatMessage>>,
    group_ids: &Arc<Mutex<Vec<String>>>,
) {
    let records = match backend.chats() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[backend] chats snapshot failed: {e:#}");
            return;
        }
    };
    eprintln!(
        "[refresh_chats] backend.chats() returned {} records (archived flags: {:?})",
        records.len(),
        records.iter().map(|r| r.archived).collect::<Vec<_>>()
    );
    let my_id = backend.account().account_id_hex.clone();
    let my_label = my_avatar_label(backend, &my_id);

    // Look up the latest message per group so the chat list shows real preview
    // text + stamps instead of empty strings.
    let metas: Vec<ChatMeta> = records
        .iter()
        .map(|r| chat_meta_from(r, backend.latest_message(&r.group_id_hex).as_ref(), &my_id, backend))
        .collect();
    let mut messages_outer: Vec<ModelRc<ChatMessage>> = Vec::with_capacity(records.len());
    let mut ids: Vec<String> = Vec::with_capacity(records.len());
    for record in &records {
        ids.push(record.group_id_hex.clone());
        // Eagerly load the most recent slice for the first chat only; the
        // others get filled on selection. Keeps boot fast for users with
        // many groups.
        let msgs = if messages_outer.is_empty() {
            backend
                .messages(&record.group_id_hex, Some(msg_window_for(&record.group_id_hex)))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let reactions = aggregate_reactions(&msgs, &my_id);
        let edits = aggregate_edits(&msgs);
        let profiles = build_sender_profiles(backend, &msgs, &my_id);
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
                chat_message_from_with_reactions(m, &by_id, &my_id, &my_label, r, e, &profiles, is_group)
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
fn merge_chat_list_rows(
    backend: &Backend,
    chats: &ModelRc<ChatMeta>,
    chats_messages: &ModelRc<ModelRc<ChatMessage>>,
    group_ids: &Arc<Mutex<Vec<String>>>,
) {
    let records = match backend.chats() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[backend] chats snapshot failed: {e:#}");
            return;
        }
    };
    let my_id = backend.account().account_id_hex.clone();
    let Some(vm) = chats.as_any().downcast_ref::<VecModel<ChatMeta>>() else {
        return;
    };
    let mut ids = group_ids.lock().unwrap();
    for r in &records {
        let meta = chat_meta_from(
            r,
            backend.latest_message(&r.group_id_hex).as_ref(),
            &my_id,
            backend,
        );
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
fn install_chat_watcher(
    backend: &Backend,
    weak: Weak<DarkMatterLinux>,
    group_ids: Arc<Mutex<Vec<String>>>,
    backend_cell: Arc<Mutex<Option<Backend>>>,
) {
    backend.watch_chats(move |record| {
        let weak = weak.clone();
        let group_ids = group_ids.clone();
        let backend_cell = backend_cell.clone();
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
            let meta = |b: Option<&Backend>| match b {
                Some(b) => chat_meta_from(&record, None, &my_id, b),
                None => fallback_chat_meta(&record),
            };
            let mut ids = group_ids.lock().unwrap();
            if let Some(pos) = ids.iter().position(|g| g == &id) {
                if let Some(vm) = chats.as_any().downcast_ref::<VecModel<ChatMeta>>() {
                    vm.set_row_data(pos, meta(guard.as_ref()));
                }
            } else {
                ids.push(id);
                if let Some(vm) = chats.as_any().downcast_ref::<VecModel<ChatMeta>>() {
                    vm.push(meta(guard.as_ref()));
                }
                if let Some(vm) = chats_messages
                    .as_any()
                    .downcast_ref::<VecModel<ModelRc<ChatMessage>>>()
                {
                    vm.push(ModelRc::new(VecModel::from(Vec::<ChatMessage>::new())));
                }
            }
            set_rail_badges(&ui, &chats);
        });
    });
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
            let prefix = if mine { "You: ".to_string() } else { String::new() };
            (format!("{prefix}{}", m.plaintext), format_unix(m.recorded_at))
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
        badge: s(""),
        read: true,
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

fn chat_message_from_with_reactions(
    record: &AppMessageRecord,
    records_by_id: &HashMap<&str, &AppMessageRecord>,
    my_account_id_hex: &str,
    my_label: &str,
    reactions: Vec<Reaction>,
    edit: Option<EditState>,
    profiles: &SenderProfiles,
    is_group: bool,
) -> ChatMessage {
    let outgoing = record.sender.eq_ignore_ascii_case(my_account_id_hex);
    // Edited messages display the latest edit's text in place of the original;
    // the "(edited)" indicator + history modal expose the change. `can_edit`
    // gates the edit affordance to the author's own bubbles.
    let edited = edit.as_ref().map(|e| e.count > 0).unwrap_or(false);
    let edit_count = edit.as_ref().map(|e| e.count as i32).unwrap_or(0);
    let display_text = edit
        .as_ref()
        .filter(|e| e.count > 0)
        .map(|e| e.text.as_str())
        .unwrap_or(record.plaintext.as_str());
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
    let key = if outgoing { my_label } else { sender_name.as_str() };
    let (a, b, init) = avatar_for(key);
    let (picture, has_picture) = bind_cached_picture(picture_url.as_deref());
    let (reply_id, reply_author, reply_text) =
        reply_preview_for(record, records_by_id, my_account_id_hex);
    let bubble_max = if outgoing { 440.0 } else { 560.0 };
    let lines = build_message_lines(display_text, bubble_max);

    // Attachment fields. Parse the NIP-92 `imeta` tag (if present) and look
    // up any decoded preview pixels we cached from a prior tap.
    let media_ref = parse_media_reference_from_tags(&record.tags, record.source_epoch);
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

    ChatMessage {
        text: s(display_text),
        lines,
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
        att_name: s(&att_name),
        att_mime: s(&att_mime),
        att_size_label: s(&att_size_label),
        att_is_image,
        att_image,
        att_has_image,
        att_loading,
        att_failed: false,
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
            (author, truncate_preview(&p.plaintext, 160))
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
    let (reply_id, reply_author, reply_text) = pending
        .reply_to
        .clone()
        .map(|(id, author, text)| (id, author, text))
        .unwrap_or_default();
    let bubble_max = 440.0_f32;
    let lines = build_message_lines(&pending.text, bubble_max);

    // Pending media optimistic-render. While the upload is in flight we
    // render the chip / image preview straight from the local bytes the user
    // picked, so the bubble doesn't pop in once the real record lands.
    let (
        has_attachment,
        att_name,
        att_mime,
        att_size_label,
        att_is_image,
        att_image,
        att_has_image,
        att_loading,
    ) = match &pending.media {
        Some(m) => {
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

    ChatMessage {
        text: s(&pending.text),
        lines,
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
        att_name: s(&att_name),
        att_mime: s(&att_mime),
        att_size_label: s(&att_size_label),
        att_is_image,
        att_image,
        att_has_image,
        att_loading,
        att_failed: pending.failed && has_attachment,
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
        let Some(pos) = find_message_row(vm, target_id) else { return };
        let Some(mut row) = vm.row_data(pos) else { return };
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
/// instant the user confirms an edit, before the kind-1010 echoes back.
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
        row.edited = true;
        row.edit_count += 1;
        vm.set_row_data(pos, row);
    });
}

/// Surgically refresh one bubble (by message id) from current snapshot +
/// overlay. Used by react/unreact and the kind-7/5 echo handler — they all
/// only need to touch the target row, not the whole model.
fn refresh_one_message_row(
    backend: &Backend,
    overlay: &PendingState,
    chats_messages: &ModelRc<ModelRc<ChatMessage>>,
    idx: usize,
    group_hex: &str,
    target_id: &str,
) {
    let my_id = backend.account().account_id_hex.clone();
    let my_label = my_avatar_label(backend, &my_id);
    let all = backend.messages(group_hex, Some(msg_window_for(group_hex))).unwrap_or_default();
    let Some(rec) = all.iter().find(|m| m.message_id_hex == target_id).cloned() else {
        return;
    };
    let mut row = build_one_message_row(&rec, &all, &my_id, &my_label, group_hex, overlay, backend);
    with_inner_messages(chats_messages, idx, |vm| {
        if let Some(pos) = find_message_row(vm, target_id) {
            preserve_grouping_flags(vm, pos, &mut row);
            vm.set_row_data(pos, row);
        }
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
    chat_message_from_with_reactions(record, &by_id, my_id, my_label, r, e, &profiles, is_group)
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
    if n > 0 {
        if let Some(mut prev) = vm.row_data(n - 1) {
            let same = (row.outgoing && prev.outgoing)
                || (!row.outgoing
                    && !prev.outgoing
                    && !row.sender_id.is_empty()
                    && prev.sender_id.as_str().eq_ignore_ascii_case(row.sender_id.as_str()));
            if same {
                grouped = true;
                prev.last_in_group = false;
                prev.show_avatar = false;
                vm.set_row_data(n - 1, prev);
            }
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

fn rebuild_chat_messages(
    backend: &Backend,
    pending: &PendingState,
    chats_messages: &ModelRc<ModelRc<ChatMessage>>,
    idx: usize,
    group_hex: &str,
) -> Vec<AppMessageRecord> {
    let t0 = std::time::Instant::now();
    let my_id = backend.account().account_id_hex.clone();
    let my_label = my_avatar_label(backend, &my_id);
    let t_label = t0.elapsed();
    let msgs = backend.messages(group_hex, Some(msg_window_for(group_hex))).unwrap_or_default();
    let t_msgs = t0.elapsed();
    let mut reactions = aggregate_reactions(&msgs, &my_id);
    apply_reaction_overlay(&mut reactions, group_hex, pending);
    let mut edits = aggregate_edits(&msgs);
    apply_edit_overlay(&mut edits, group_hex, pending);
    let profiles = build_sender_profiles(backend, &msgs, &my_id);
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
            let r = reactions
                .get(&m.message_id_hex)
                .cloned()
                .unwrap_or_default();
            let e = edits.get(&m.message_id_hex).cloned();
            chat_message_from_with_reactions(m, &by_id, &my_id, &my_label, r, e, &profiles, is_group)
        })
        .collect();

    let pending_count = pending.sends.get(group_hex).map(|p| p.len()).unwrap_or(0);
    if let Some(pendings) = pending.sends.get(group_hex) {
        for p in pendings {
            rows.push(pending_chat_message(p, &my_id, &my_label));
        }
    }

    let keys = grouping_keys(&msgs, &my_id, pending_count);
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
    // Hand the snapshot back so the caller can reuse it (avatar fetches on
    // chat open) instead of paying for a second backend read of the same
    // window of records.
    msgs
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

/// Walk all records and resolve kind-1010 edits per target message.
///
/// Authorship is enforced here: an edit is only honored when its authenticated
/// author (the inner event's `sender`, which marmot guarantees equals the
/// MLS-authenticated sender) matches the *original* message's author. A
/// kind-1010 from anyone else referencing your message is ignored. Edits are
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
        if r.kind != 1010 {
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
/// optimistic edit shows before its kind-1010 echoes back. Mirrors
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
        .filter(|r| r.kind == 1010)
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
    if s.len() <= 6 { s.to_string() } else { s[..6].to_string() }
}

// ─── Emoji catalog ──────────────────────────────────────────────────────

/// Auto-generated by build.rs — exposes `EMOJI_POSITIONS`, `TILE`,
/// `SHEET_WIDTH`, `SHEET_HEIGHT`, `COUNT`.
mod emoji_sprite_map {
    include!(concat!(env!("OUT_DIR"), "/emoji_sprite_map.rs"));
}

/// The composed sprite PNG, embedded as bytes by build.rs.
const EMOJI_SPRITE_PNG: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/twemoji_sprite.png"));

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
        link: link
            .as_deref()
            .map(SharedString::from)
            .unwrap_or_default(),
    }
}

fn md_run_emoji(x: u32, y: u32) -> MessageRun {
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
                out.push(MdTok::Emoji { x, y });
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
                let st = MdStyle { code: true, ..style };
                md_push_text(out, s, st, &link, positions, false);
            }
            Inline::Emph(c) => {
                md_walk_inlines(out, c, MdStyle { italic: true, ..style }, link.clone(), positions)
            }
            Inline::Strong(c) => {
                md_walk_inlines(out, c, MdStyle { bold: true, ..style }, link.clone(), positions)
            }
            Inline::Strikethrough(c) => md_walk_inlines(
                out,
                c,
                MdStyle { strike: true, ..style },
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
                let st = MdStyle { code: true, ..style };
                md_push_text(out, s, st, &link, positions, false);
            }
            Inline::NostrMention(e) => md_push_nostr(out, e, style, true),
            Inline::NostrUri(e) => md_push_nostr(out, e, style, false),
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
            MdTok::Emoji { x: ex, y: ey } => {
                if x > 0.0 && x + emoji_w > avail {
                    flush(out, &mut cur);
                    x = 0.0;
                }
                cur.push(md_run_emoji(ex, ey));
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
            MdStyle { bold: header, ..Default::default() },
            None,
            positions,
        );
        toks.push(MdTok::Space {
            text: " ".to_string(),
            style: MdStyle::default(),
            link: None,
        });
    }
    md_wrap(out, toks, max_width, base_fs, ctx.indent, 1.0, ctx.quote, false);
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
                md_wrap(out, toks, max_width, base_fs, ctx.indent, 1.0, ctx.quote, false);
            }
            Block::Heading { level, inlines } => {
                let mut toks = Vec::new();
                md_walk_inlines(
                    &mut toks,
                    inlines,
                    MdStyle { bold: true, ..Default::default() },
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
                let st = MdStyle { code: true, ..Default::default() };
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
                    md_wrap(out, toks, max_width, base_fs, ctx.indent, 1.0, ctx.quote, true);
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
                md_walk_list(out, *kind, *tight, items, ctx, max_width, base_fs, positions);
            }
            Block::Table { header, rows, .. } => {
                md_emit_table_row(out, header, true, ctx, max_width, base_fs, positions);
                for row in rows {
                    md_emit_table_row(out, row, false, ctx, max_width, base_fs, positions);
                }
            }
            Block::MathBlock { content } => {
                let body = content.strip_suffix('\n').unwrap_or(content);
                let st = MdStyle { code: true, ..Default::default() };
                for line in body.split('\n') {
                    let mut toks = Vec::new();
                    md_push_text(&mut toks, line, st, &None, positions, false);
                    md_wrap(out, toks, max_width, base_fs, ctx.indent, 1.0, ctx.quote, true);
                }
            }
        }
    }
}

/// Parse a chat-message body as Markdown and flatten it into pre-wrapped lines.
fn tokenize_message_lines(text: &str, max_width: f32, base_fs: f32) -> Vec<MessageLine> {
    let positions = emoji_position_index();
    let doc = whitenoise_markdown::parse(text);
    let mut lines: Vec<MdLine> = Vec::new();
    md_walk_blocks(
        &mut lines,
        &doc.blocks,
        MdCtx { indent: 0.0, quote: 0 },
        max_width,
        base_fs,
        positions,
    );
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
    if at > end || end > draft.len() || !draft.is_char_boundary(at) || !draft.is_char_boundary(end) {
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

/// Push `text` to the system clipboard.
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
fn refresh_kp_local(backend: &Backend, ui: &DarkMatterLinux) {
    let local = backend.key_packages_local();
    // "Published" means actually out on the network — a KP with a published
    // event id, or one we've observed on a relay. A purely local KP (which
    // always exists once the account boots) does NOT count.
    let published = local
        .iter()
        .any(|kp| !kp.key_package_event_id.is_empty() || kp.relay);
    ui.set_kp_published(published);
    let rows: Vec<KeyPackageInfo> = local.iter().map(kp_to_ui).collect();
    ui.set_key_packages(ModelRc::new(VecModel::from(rows)));
    let relays: Vec<SharedString> = backend
        .key_package_relays()
        .into_iter()
        .map(SharedString::from)
        .collect();
    ui.set_kp_relays(ModelRc::new(VecModel::from(relays)));
}

/// Push the on-disk relay list into the UI model. Used after add/remove.
fn push_network_relays(ui: &DarkMatterLinux, list: &[String]) {
    let rows: Vec<SharedString> = list.iter().cloned().map(SharedString::from).collect();
    ui.set_network_relays(ModelRc::new(VecModel::from(rows)));
    // Keep the one-click suggestions in sync: only offer ones not already added.
    push_suggested_relays(ui, list);
}

/// Well-known public relays offered as one-click adds on the get-started screen.
const SUGGESTED_RELAYS: &[&str] = &["wss://relay.primal.net", "wss://relay.ditto.pub"];

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
fn refresh_network_post_boot(backend: &Backend, ui: &DarkMatterLinux) {
    let booted: Vec<SharedString> = backend
        .booted_relays()
        .iter()
        .cloned()
        .map(SharedString::from)
        .collect();
    ui.set_network_booted_relays(ModelRc::new(VecModel::from(booted)));
    let (connected, total) = backend.relay_health();
    ui.set_network_connected(connected as i32);
    ui.set_network_total(total as i32);
    // Mark the first sync so the chat-list footer leaves "SYNCING…" and the 1s
    // timer starts counting up from a real baseline.
    ui.set_sync_secs(0);
}

/// `Mon DD · HH:MM` for KP timestamps. Returns "" for zero (unknown).
fn format_date_unix(secs: u64) -> String {
    if secs == 0 {
        return String::new();
    }
    let days_since_epoch = (secs / 86_400) as i64;
    let (y, m, d) = civil_from_days(days_since_epoch);
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mi = (m as usize).saturating_sub(1).min(11);
    let _ = y;
    let secs_of_day = secs % 86_400;
    let h = (secs_of_day / 3600) % 24;
    let mn = (secs_of_day / 60) % 60;
    format!("{} {} · {:02}:{:02}", months[mi], d, h, mn)
}

// Howard Hinnant's date algorithm (public domain). days from 1970-01-01.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

/// Render a unix-seconds timestamp as `HH:MM` (UTC). Good enough for v1.
fn format_unix(secs: u64) -> String {
    let secs_of_day = secs % 86_400;
    let h = (secs_of_day / 3600) % 24;
    let m = (secs_of_day / 60) % 60;
    format!("{h:02}:{m:02}")
}

// ─── Contacts ──────────────────────────────────────────────────────────

fn refresh_contacts(
    backend: &Backend,
    contacts: &ModelRc<Contact>,
    nicknames: &std::collections::BTreeMap<String, String>,
) {
    let records = match backend.follow_list() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[backend] follow_list failed: {e:#}");
            return;
        }
    };
    let rows: Vec<Contact> = records.iter().map(|r| contact_from(r, nicknames)).collect();
    if let Some(vm) = contacts.as_any().downcast_ref::<VecModel<Contact>>() {
        vm.set_vec(rows);
    }
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
    let display = if nickname.is_empty() { published.clone() } else { nickname.clone() };
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
fn push_group_settings_to_ui(ui: &DarkMatterLinux, backend: &Backend, group_hex: &str) {
    if let Ok(mut slot) = active_group_slot().lock() {
        *slot = group_hex.to_string();
    }
    let records = backend.chats().unwrap_or_default();
    let rec = records
        .iter()
        .find(|g| g.group_id_hex.eq_ignore_ascii_case(group_hex));
    let count = backend.group_member_count(group_hex);
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

fn push_group_members_to_ui(ui: &DarkMatterLinux, backend: &Backend, group_hex: &str) {
    push_group_settings_to_ui(ui, backend, group_hex);
    let count = backend.group_member_count(group_hex);
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
    let viewer_is_admin = backend.is_group_admin(group_hex);
    ui.set_chat_can_add_members(viewer_is_admin);
    let admins = backend.group_admins(group_hex);
    let my_id = backend.account().account_id_hex.clone();
    // Build (row, picture_url) pairs so we can spawn per-member image fetches
    // after the initial paint. URLs are looked up once here and shipped to
    // the worker tasks; the model itself only carries the decoded image.
    let mut pairs: Vec<(GroupMember, Option<String>)> = match backend.group_members(group_hex) {
        Ok(members) => members
            .iter()
            .map(|m| group_member_from(backend, m, &my_id, &admins, viewer_is_admin))
            .collect(),
        Err(e) => {
            eprintln!("[members] {e:#}");
            Vec::new()
        }
    };
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
fn spawn_message_avatar_fetches(ui: &DarkMatterLinux, backend: &Backend, msgs: &[AppMessageRecord]) {
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
        let Some(mut row) = vm.row_data(i) else { continue };
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
fn spawn_chat_list_avatar_fetches(ui: &DarkMatterLinux, backend: &Backend) {
    let chats = match backend.chats() {
        Ok(c) => c,
        Err(_) => return,
    };
    for record in chats {
        let Some(peer) = backend.direct_chat_peer(&record.group_id_hex) else {
            continue;
        };
        let (_, url) = backend.account_name_and_picture(&peer);
        let Some(url) = url
            .map(|u| u.trim().to_string())
            .filter(|u| !u.is_empty())
        else {
            continue;
        };
        if picture_cache_has(&url) {
            continue;
        }
        let npub = format!("mls:0x{}", short_hex(&record.group_id_hex));
        let weak = ui.as_weak();
        backend.tokio_handle().spawn(async move {
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
}

/// Bind a decoded picture onto the chat-list row identified by `npub`.
fn update_chat_picture(ui: &DarkMatterLinux, npub: &str, pixels: &PicturePixels) {
    let chats = ui.get_chats();
    let Some(vm) = chats.as_any().downcast_ref::<VecModel<ChatMeta>>() else {
        return;
    };
    let img = rgba_to_slint_image(pixels);
    for i in 0..vm.row_count() {
        let Some(mut row) = vm.row_data(i) else { continue };
        if row.npub.as_str() != npub {
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
        let Some(mut row) = vm.row_data(i) else { continue };
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
    let is_self = record
        .member_id_hex
        .eq_ignore_ascii_case(my_account_id_hex);
    let is_admin = admins
        .iter()
        .any(|a| a.eq_ignore_ascii_case(&record.member_id_hex));
    // The viewer can hand out admin only to other members who aren't admins yet.
    let can_promote = viewer_is_admin && !is_self && !is_admin;
    // Demote another admin, or step down from one's own admin role.
    let can_demote = viewer_is_admin && is_admin && !is_self;
    let can_self_demote = viewer_is_admin && is_admin && is_self;
    let mut name = backend.account_display_name(&record.member_id_hex);
    if let Some(label) = record.account.as_ref().filter(|s| !s.is_empty()) {
        if name.starts_with("0x") {
            name = label.clone();
        }
    }
    let npub = npub_for_account_id(&record.member_id_hex).unwrap_or_else(|_| record.member_id_hex.clone());
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
    Ok(PicturePixels { w, h, rgba: img.into_raw() })
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

fn refresh_archived(
    backend: &Backend,
    archived: &ModelRc<ArchivedChat>,
    archived_group_ids: &Arc<Mutex<Vec<String>>>,
) {
    let records = match backend.archived_chats() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[backend] archived_chats failed: {e:#}");
            return;
        }
    };
    let my_id = backend.account().account_id_hex.clone();
    let mut ids = Vec::with_capacity(records.len());
    let rows: Vec<ArchivedChat> = records
        .iter()
        .map(|r| {
            ids.push(r.group_id_hex.clone());
            archived_from(
                r,
                backend.latest_message(&r.group_id_hex).as_ref(),
                &my_id,
                backend,
            )
        })
        .collect();
    *archived_group_ids.lock().unwrap() = ids;
    if let Some(vm) = archived.as_any().downcast_ref::<VecModel<ArchivedChat>>() {
        vm.set_vec(rows);
    }
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
            let prefix = if mine { "You: ".to_string() } else { String::new() };
            (format!("{prefix}{}", m.plaintext), format_unix(m.recorded_at))
        }
        None => (record.profile.description.clone(), String::new()),
    };
    ArchivedChat {
        name: s(&name),
        last_msg: s(&last_msg),
        last_date: s(&last_date),
        av_a: a,
        av_b: b,
        av_initials: s(&init),
        members: backend.group_member_count(&record.group_id_hex) as i32,
        group_id: s(&format!("mls:0x{}", short_hex(&record.group_id_hex))),
    }
}

// ─── Per-chat live message watcher ─────────────────────────────────────

/// Attach a watcher that appends new messages into the inner messages model
/// for the currently-open chat. Caller is responsible for aborting the
/// returned `JoinHandle` when the user switches chats.
fn install_message_watcher(
    backend: &Backend,
    weak: Weak<DarkMatterLinux>,
    backend_cell: Arc<Mutex<Option<Backend>>>,
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
        if !matches!(kind, 9 | 7 | 5 | 1010) {
            return;
        }
        let weak = weak.clone();
        let backend_cell = backend_cell.clone();
        let pending_state = pending_state.clone();
        let group_hex_inner = group_hex_for_filter.clone();
        let msg_id = received.message_id_hex.clone();
        let target_id_for_reaction: Option<String> = if kind == 7 || kind == 5 || kind == 1010 {
            received
                .tags
                .iter()
                .find(|t| t.len() >= 2 && t[0] == "e")
                .map(|t| t[1].clone())
        } else {
            None
        };
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            let guard = backend_cell.lock().unwrap();
            let Some(b) = guard.as_ref() else { return };
            let overlay = pending_state.lock().unwrap();
            let chats_messages = ui.get_chats_messages();

            match kind {
                9 => {
                    // Chat message echo. If the row already exists (because
                    // we just reconciled our own send), do nothing. Otherwise
                    // append it surgically — no full rebuild.
                    let my_id = b.account().account_id_hex.clone();
                    let my_label = my_avatar_label(b, &my_id);
                    let all = b.messages(&group_hex_inner, Some(msg_window_for(&group_hex_inner))).unwrap_or_default();
                    let Some(rec) = all
                        .iter()
                        .find(|m| m.message_id_hex == msg_id)
                        .cloned()
                    else {
                        return;
                    };
                    let row = build_one_message_row(&rec, &all, &my_id, &my_label, &group_hex_inner, &overlay, b);
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
                        spawn_message_avatar_fetches(&ui, b, &all);
                    }
                }
                7 | 5 | 1010 => {
                    // Reaction, delete, or edit — surgical refresh of the
                    // target row. For an edit the snapshot now carries the
                    // kind-1010, so the rebuilt row picks up the new text.
                    let Some(target) = target_id_for_reaction else { return };
                    refresh_one_message_row(
                        b,
                        &overlay,
                        &chats_messages,
                        chat_idx,
                        &group_hex_inner,
                        &target,
                    );
                }
                _ => {}
            }
        });
    })
}
