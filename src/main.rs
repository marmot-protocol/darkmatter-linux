// These imports are re-exported `pub(crate)` so the extracted UI submodules
// (`md`, `chatmodel`, `media`, `profile`, `chrome`, `wiring`, …) can pull the
// whole shared surface with a single `use crate::*;` instead of re-listing
// every dependency. `pub(crate) use` re-exports are exempt from the
// unused-import lint, so this stays clean under the `-D warnings` commit gate.
pub(crate) use std::cell::RefCell;
pub(crate) use std::collections::{HashMap, HashSet};
pub(crate) use std::rc::Rc;
pub(crate) use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
pub(crate) use std::sync::{Arc, Mutex, OnceLock};

pub(crate) use marmot_app::{
    AppGroupMemberRecord, AppGroupRecord, AppMessageRecord, AuditLogFile, MediaAttachmentReference,
    MediaLocator, UserDirectoryRecord, UserProfileMetadata, npub_for_account_id,
};
pub(crate) use nostr::Keys;
pub(crate) use nostr::nips::nip19::ToBech32;
pub(crate) use slint::{Color, Model, ModelRc, SharedString, VecModel, Weak};
pub(crate) use tokio::task::JoinHandle;

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

// UI-glue submodules carved out of main.rs to keep every file under 2000
// lines. Each shares the crate-root prelude via `use crate::*;`; we re-export
// their items back so main.rs and the sibling submodules see them flat, exactly
// as when everything lived in one file.
mod chatlist;
pub(crate) use chatlist::*;
mod chatmodel;
pub(crate) use chatmodel::*;
mod chrome;
pub(crate) use chrome::*;
mod media;
pub(crate) use media::*;
mod render;
pub(crate) use render::*;
mod state;
pub(crate) use state::*;
mod wiring;
pub(crate) use wiring::*;

pub(crate) use backend::Backend;
pub(crate) use settings::Settings;
pub(crate) use vault::Vault;

// Tests that point `DM_HOME` at a temp dir mutate a single process-global env
// var, so the vault and backup suites must not run concurrently — they share
// this lock to serialize. (Poisoning is ignored: a panicking test still leaves
// the lock usable for the next.)
#[cfg(test)]
pub(crate) static DM_HOME_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// Generated Slint UI (components, ui/tokens.slint structs, globals) plus the
// build-time emoji sprite artifacts — all owned by the wnl-ui crate so Rust
// edits here don't recompile the generated UI module.
pub(crate) use wnl_ui::*;

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
    // Seed the in-memory per-account "delete for me" sets so locally-hidden
    // messages stay hidden across restarts. The renderer's current-account
    // pointer (`hidden_set_account`) is set once boot resolves the active
    // account. Legacy global hides are stashed and folded into the boot account
    // there too, so a hide on one account never leaks to another.
    for (acct, ids) in &initial_settings.hidden_messages_by_account {
        hidden_init(acct, ids.iter().cloned());
    }
    if !initial_settings.hidden_messages_legacy.is_empty() {
        hidden_stash_legacy(
            initial_settings
                .hidden_messages_legacy
                .iter()
                .cloned()
                .collect(),
        );
    }
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

    // Bundle the shared handles so each `wire_*` function takes a single `&cx`
    // and reproduces the local bindings it needs via `let Cx { .. } = cx.clone()`.
    let cx = Cx {
        notif: notif.clone(),
        settings_cell: settings_cell.clone(),
        contacts: contacts.clone(),
        chats_messages: chats_messages.clone(),
        backend_cell: backend_cell.clone(),
        vault_cell: vault_cell.clone(),
        group_ids: group_ids.clone(),
        archived_group_ids: archived_group_ids.clone(),
        pending_state: pending_state.clone(),
        staged_files: staged_files.clone(),
        active_message_watcher: active_message_watcher.clone(),
        chats_watcher: chats_watcher.clone(),
    };

    let refresh_breadcrumb: SharedFn = {
        let weak = ui.as_weak();
        Arc::new(move || {
            let Some(ui) = weak.upgrade() else { return };
            refresh_breadcrumb_now(&ui);
        })
    };

    let refresh_storage_size: SharedFn = {
        let weak = ui.as_weak();
        Arc::new(move || {
            let weak = weak.clone();
            std::thread::spawn(move || {
                let label = human_bytes(media_cache::size_bytes());
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_storage_cache_size(label.into());
                    }
                });
            });
        })
    };

    let refresh_all_chat_models: SharedFn = {
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        let archived_group_ids = archived_group_ids.clone();
        Arc::new(move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(b) = backend_cell.lock().unwrap().clone() else {
                return;
            };
            refresh_all_chat_models_async(&ui, &b, &group_ids, &archived_group_ids);
        })
    };

    let dispatch_send: DispatchSendFn = {
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

    let edit_op: EditOpFn = {
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

    // Bundle the cross-section closures so each `wire_*` takes a single `&h`.
    let h = Handlers {
        refresh_breadcrumb: refresh_breadcrumb.clone(),
        refresh_storage_size: refresh_storage_size.clone(),
        refresh_all_chat_models: refresh_all_chat_models.clone(),
        dispatch_send: dispatch_send.clone(),
        edit_op: edit_op.clone(),
    };

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
    let boot_backend: BootFn = {
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
        Arc::new(
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
                                // Point the "delete for me" renderer at the booted
                                // account and fold any pre-upgrade global hides into
                                // it — before the first populate so hidden rows are
                                // filtered on the very first paint.
                                let me = b.account().account_id_hex.clone();
                                hidden_set_account(&me);
                                hidden_init(&me, hidden_take_legacy());
                                // Every list is fetched on the backend runtime and
                                // applied back on the UI thread — the boot closure
                                // itself does zero sqlite/disk reads.
                                populate_models_for_active(
                                    &ui,
                                    &b,
                                    &group_ids,
                                    &archived_group_ids,
                                );
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
                                        let key =
                                            group_ids_banner.lock().unwrap().get(idx).cloned();
                                        trigger_encryption_banner_entrance(
                                            &ui,
                                            key.as_deref(),
                                            &banner_seen_boot,
                                        );
                                        // The default-selected chat is shown at
                                        // boot without a `chat-selected` click, so
                                        // restore its saved draft here. Only when
                                        // the composer is still untouched, so we
                                        // never clobber typing started meanwhile.
                                        if let Some(key) = key
                                            && ui.get_composer_draft().is_empty()
                                        {
                                            let draft = Settings::load().draft(&key).to_string();
                                            if !draft.is_empty() {
                                                ui.set_composer_draft(s(&draft));
                                            }
                                        }
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
                                            if sync_done.load(std::sync::atomic::Ordering::Relaxed)
                                            {
                                                return;
                                            }
                                            let Some(ui) = weak.upgrade() else { return };
                                            let Some(b) = backend_cell.lock().unwrap().clone()
                                            else {
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
            },
        )
    };

    wire_panes(
        &ui,
        &cx,
        &h,
        &boot_backend,
        &pending_generated,
        &pending_profile_seed,
        &pending_profile_name,
    );
    wire_backup(&ui, &cx, &h, &encryption_banner_seen);
    wire_messaging(&ui, &cx, &h);
    wire_extra(&ui, &cx, &h);

    ui.run()?;

    // The window is closed but `ui` is still alive: flush the on-screen chat's
    // unsent draft so quitting (without a chat switch or send to trigger the
    // other save paths) still preserves a half-written message for next launch.
    if ui.get_editing_message_id().is_empty() {
        let idx = ui.get_active_chat();
        if let Some(group_hex) = cx.group_ids.lock().unwrap().get(idx as usize).cloned() {
            let mut st = cx.settings_cell.borrow_mut();
            if st.set_draft(&group_hex, &ui.get_composer_draft()) {
                st.save();
            }
        }
    }
    Ok(())
}
