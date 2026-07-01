use crate::*;

pub(crate) fn wire_panes(
    ui: &DarkMatterLinux,
    cx: &Cx,
    h: &Handlers,
    boot_backend: &BootFn,
    pending_generated: &Arc<Mutex<Option<String>>>,
    pending_profile_seed: &Arc<Mutex<Option<String>>>,
    pending_profile_name: &Arc<Mutex<Option<String>>>,
) {
    let Cx {
        notif,
        settings_cell,
        backend_cell,
        vault_cell,
        group_ids,
        archived_group_ids,
        pending_state,
        staged_files,
        active_message_watcher,
        chats_watcher,
        ..
    } = cx.clone();
    let Handlers {
        refresh_breadcrumb,
        refresh_storage_size,
        ..
    } = h.clone();
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
            // Point the "delete for me" renderer at the new account's hidden set
            // before any rows rebuild, so hides don't leak across the switch.
            hidden_set_account(&summary.account_id_hex);
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
            // Backfill the built-in "Saved Messages" chat for accounts that have
            // never had one (off-thread; pins itself in once created).
            ensure_self_chat_async(&ui, &backend, &group_ids);
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

    // Right-click a rail chat row: resolve the row's group id, read its live
    // pin + mute state (Rust owns both sets), and open the context menu at the
    // cursor. The menu itself is Slint; only the state lookup needs Rust.
    ui.on_request_chat_context({
        let weak = ui.as_weak();
        let group_ids = group_ids.clone();
        let notif = notif.clone();
        move |idx, ax, ay| {
            let Some(ui) = weak.upgrade() else { return };
            let group_hex = group_ids.lock().unwrap().get(idx as usize).cloned();
            let Some(group_hex) = group_hex else { return };
            ui.set_chat_ctx_idx(idx);
            ui.set_chat_ctx_x(ax);
            ui.set_chat_ctx_y(ay);
            ui.set_chat_ctx_pinned(is_pinned(&group_hex));
            ui.set_chat_ctx_muted(notif.is_muted(&group_hex));
            ui.set_chat_ctx_open(true);
        }
    });

    // Pin / unpin a chat to the top of the rail. Flips the live pinned set +
    // the persisted settings, then re-sorts the chat list — keeping whatever
    // chat is currently open selected across the reorder.
    ui.on_toggle_pin_chat({
        let weak = ui.as_weak();
        let group_ids = group_ids.clone();
        let settings_cell = settings_cell.clone();
        let backend_cell = backend_cell.clone();
        move |idx| {
            let Some(ui) = weak.upgrade() else { return };
            let group_hex = group_ids.lock().unwrap().get(idx as usize).cloned();
            let Some(group_hex) = group_hex else { return };
            let now_pinned = toggle_pinned(&group_hex);
            {
                let mut s = settings_cell.borrow_mut();
                if now_pinned {
                    s.pinned_chats.insert(group_hex.clone());
                } else {
                    s.pinned_chats.remove(&group_hex);
                }
                s.save();
            }
            let Some(backend) = backend_cell.lock().unwrap().clone() else {
                return;
            };
            // Re-order in place (preserving loaded messages + the open chat),
            // rather than a full refresh which would blank the conversation.
            reorder_chats_by_pin_async(&ui, &backend, &group_ids);
        }
    });

    // Mute / unmute a specific rail row (from its context menu) — same effect
    // as the header bell, but targets the right-clicked chat by index rather
    // than the open one. Keeps the header in sync when they coincide.
    ui.on_toggle_mute_chat_at({
        let weak = ui.as_weak();
        let group_ids = group_ids.clone();
        let settings_cell = settings_cell.clone();
        let notif = notif.clone();
        move |idx| {
            let Some(ui) = weak.upgrade() else { return };
            let group_hex = group_ids.lock().unwrap().get(idx as usize).cloned();
            let Some(group_hex) = group_hex else { return };
            let now_muted = !notif.is_muted(&group_hex);
            notif.set_muted(&group_hex, now_muted);
            {
                let mut s = settings_cell.borrow_mut();
                if now_muted {
                    s.muted_chats.insert(group_hex.clone());
                } else {
                    s.muted_chats.remove(&group_hex);
                }
                s.save();
            }
            if idx == ui.get_active_chat() {
                ui.set_active_chat_muted(now_muted);
            }
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
        push_network_relays(ui, &initial);
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
    refresh_breadcrumb();

    // Recompute the Storage pane's media-cache size off the UI thread (disk
    // walk) and push the formatted label back. Cheap, but IO — never inline.
    refresh_storage_size();
    // Static for the session — the data dir doesn't move while we're running.
    ui.set_storage_vault_dir(vault::vault_dir().display().to_string().into());

    // Reveal the folder holding vault.db in the platform file manager. Reuses the
    // same xdg-open/open handler as external links — a directory path is fine.
    ui.on_storage_open_vault_folder(move || {
        open_external(&vault::vault_dir().display().to_string());
    });
}
