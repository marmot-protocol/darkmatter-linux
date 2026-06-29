use crate::*;

pub(crate) fn wire_backup(
    ui: &DarkMatterLinux,
    cx: &Cx,
    h: &Handlers,
    encryption_banner_seen: &Arc<Mutex<std::collections::HashSet<String>>>,
) {
    let Cx {
        notif,
        settings_cell,
        contacts,
        backend_cell,
        vault_cell,
        group_ids,
        archived_group_ids,
        pending_state,
        active_message_watcher,
        ..
    } = cx.clone();
    let Handlers {
        refresh_breadcrumb,
        refresh_storage_size,
        refresh_all_chat_models,
        ..
    } = h.clone();
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
    // Contact detail → "Show as QR": rasterize the contact's nostr:npub and
    // open the QrModal. Reuses `qr_image` (UI-thread only — Image is !Send).
    ui.on_contact_show_qr({
        let weak = ui.as_weak();
        let contacts = contacts.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(row) = contacts.row_data(ui.get_active_contact() as usize) else {
                return;
            };
            let npub = row.npub_full.to_string();
            if npub.is_empty() {
                return;
            }
            ui.set_contact_qr(qr_image(&format!("nostr:{npub}")));
            ui.set_contact_qr_npub(s(&npub));
            ui.set_contact_qr_npub_short(row.npub_short.clone());
            ui.set_contact_qr_name(row.name.clone());
            ui.set_contact_qr_open(true);
        }
    });
    ui.on_contact_qr_dismissed({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_contact_qr_open(false);
            }
        }
    });
    // Contact detail → "Refresh" key package: re-fetch the peer's latest key
    // package from their relays off-thread, then patch the row with the real
    // freshness state (matched by account-id, since the index may have moved).
    ui.on_contact_refresh_key_package({
        let weak = ui.as_weak();
        let contacts = contacts.clone();
        let backend_cell = backend_cell.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let idx = ui.get_active_contact() as usize;
            let Some(mut row) = contacts.row_data(idx) else {
                return;
            };
            let account_id = row.account_id.to_string();
            if account_id.is_empty() {
                return;
            }
            let Some(b) = backend_cell.lock().unwrap().clone() else {
                return;
            };
            // Honest in-flight state instead of a frozen placeholder.
            row.kp_status = s("Checking…");
            row.kp_detail = s("Contacting relays…");
            contacts.set_row_data(idx, row);

            let weak = ui.as_weak();
            std::thread::spawn(move || {
                let result = b.fetch_contact_key_package(&account_id);
                let (status, detail) = match result {
                    Ok((created_at, relays)) => kp_labels(created_at, &relays),
                    Err(e) => {
                        eprintln!("[backend] fetch_contact_key_package failed: {e:#}");
                        (
                            "Not found".to_string(),
                            "No key package on relays yet".to_string(),
                        )
                    }
                };
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    let contacts = ui.get_contacts();
                    let Some(vm) = contacts.as_any().downcast_ref::<VecModel<Contact>>() else {
                        return;
                    };
                    for i in 0..vm.row_count() {
                        let Some(mut r) = vm.row_data(i) else {
                            continue;
                        };
                        if r.account_id != account_id {
                            continue;
                        }
                        r.kp_status = s(&status);
                        r.kp_detail = s(&detail);
                        vm.set_row_data(i, r);
                        break;
                    }
                });
            });
        }
    });
    // Contact detail → "Start chat": create a 1:1 conversation with the
    // selected contact and drop the user into it. Mirrors the new-chat
    // modal's single-member path (`create_group` with one npub, empty
    // name) but skips the modal on success — failures surface there
    // instead, since the contact page has no inline status line.
    ui.on_start_chat_with_contact({
        let weak = ui.as_weak();
        let contacts = contacts.clone();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(row) = contacts.row_data(ui.get_active_contact() as usize) else {
                return;
            };
            let npub = row.npub_full.to_string();
            if npub.is_empty() {
                return;
            }
            let Some(b) = backend_cell.lock().unwrap().clone() else {
                return;
            };
            // A contact can't be the user themselves, but guard anyway —
            // marmot rejects self-invites.
            let me_npub = npub_for_account_id(&b.account().account_id_hex).ok();
            if me_npub
                .as_deref()
                .map(|n| n.eq_ignore_ascii_case(&npub))
                .unwrap_or(false)
            {
                return;
            }
            let weak = weak.clone();
            let group_ids = group_ids.clone();
            std::thread::spawn(move || {
                let result = b.create_group("", std::slice::from_ref(&npub));
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    match result {
                        Ok(group_id) => {
                            let group_hex = hex::encode(group_id.as_slice());
                            refresh_chats_async(&ui, &b, &group_ids, move |ui, _b, snap| {
                                let pos = snap
                                    .records
                                    .iter()
                                    .position(|r| r.group_id_hex.eq_ignore_ascii_case(&group_hex));
                                if let Some(pos) = pos {
                                    // Arriving from the Contacts page — switch
                                    // to Chats so the new conversation is
                                    // visible, then select it.
                                    ui.set_active_page(Page::Chats as i32);
                                    refresh_breadcrumb_now(ui);
                                    ui.set_active_chat(pos as i32);
                                    ui.invoke_chat_selected(pos as i32);
                                }
                            });
                        }
                        Err(e) => {
                            eprintln!("[start-chat-with-contact] {e:#}");
                            // No status line on the contact page, so surface
                            // the error in the new-chat modal (pre-filled
                            // with the member) where the user can adjust or
                            // retry.
                            ui.set_new_chat_name(s(""));
                            ui.set_new_chat_members(s(&npub));
                            ui.set_new_chat_busy(false);
                            ui.set_new_chat_status(friendly_error("create chat", &e).into());
                            ui.set_show_new_chat(true);
                        }
                    }
                });
            });
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
}
