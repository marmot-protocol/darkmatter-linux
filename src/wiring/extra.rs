use crate::*;

pub(crate) fn wire_extra(ui: &DarkMatterLinux, cx: &Cx, h: &Handlers) {
    let Cx {
        settings_cell,
        backend_cell,
        vault_cell,
        group_ids,
        pending_state,
        ..
    } = cx.clone();
    let Handlers {
        refresh_all_chat_models,
        dispatch_send,
        ..
    } = h.clone();
    wire_reply_target(ui);
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

    // ─── Delete for everyone (kind-5 retraction, optimistic) ───────────
    //
    // Same optimistic shape as `edit_op`: stamp the overlay, tombstone the row
    // immediately, publish the kind-5 in the background, then on ack drop the
    // overlay (the snapshot now carries the delete) and on failure drop it +
    // refresh so the row reverts to its confirmed content.
    ui.on_request_delete_everyone({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        let pending_state = pending_state.clone();
        move |message_id| {
            let Some(ui) = weak.upgrade() else { return };
            let target = message_id.to_string();
            if target.is_empty() {
                return;
            }
            let idx = ui.get_active_chat() as usize;
            let Some(group_hex) = group_ids.lock().unwrap().get(idx).cloned() else {
                return;
            };

            // 1. Optimistic overlay + tombstone the row now.
            {
                let mut overlay = pending_state.lock().unwrap();
                overlay.deletes.insert((group_hex.clone(), target.clone()));
            }
            let guard = backend_cell.lock().unwrap();
            let Some(backend) = guard.as_ref() else {
                return;
            };
            refresh_one_message_row_async(
                backend,
                ui.as_weak(),
                pending_state.clone(),
                group_ids.clone(),
                group_hex.clone(),
                target.clone(),
            );

            // 2. Dispatch + reconcile (surgical).
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
                            eprintln!("[delete] {e:#}");
                            ui.set_backend_error(friendly_error("delete", e).into());
                        }
                        overlay.deletes.remove(&(group_hex.clone(), target.clone()));
                    }
                    let Some(backend) = backend_cell.lock().unwrap().clone() else {
                        return;
                    };
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
            backend.delete_message_async(&group_hex, &target, on_done);
        }
    });

    // ─── Delete for me (local-only hide) ───────────────────────────────
    //
    // Never touches the wire: record the id in the persisted hidden set + the
    // in-memory global the renderer consults, then rebuild the active chat so
    // the row drops out. Works on any message (own or others').
    ui.on_request_delete_me({
        let weak = ui.as_weak();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        let pending_state = pending_state.clone();
        let settings_cell = settings_cell.clone();
        move |message_id| {
            let Some(ui) = weak.upgrade() else { return };
            let id = message_id.to_string();
            if id.is_empty() {
                return;
            }
            // Rebuild the active chat so the now-hidden row disappears. Window
            // read rides the backend runtime; the UI thread never hits sqlite.
            let idx = ui.get_active_chat() as usize;
            let Some(group_hex) = group_ids.lock().unwrap().get(idx).cloned() else {
                return;
            };
            let guard = backend_cell.lock().unwrap();
            let Some(backend) = guard.as_ref() else {
                return;
            };
            // Scope the hide to the *active account* — never the machine — so a
            // second account on this device still sees the message.
            let my_id = backend.account().account_id_hex;
            if hidden_insert(&my_id, &id) {
                let mut st = settings_cell.borrow_mut();
                st.hide_message(&my_id, &id);
                st.save();
            }
            let weak2 = ui.as_weak();
            let pending_state = pending_state.clone();
            let group_ids2 = group_ids.clone();
            let b = backend.clone();
            backend.tokio_handle().spawn(async move {
                let msgs = b
                    .messages(&group_hex, Some(msg_window_for(&group_hex)))
                    .unwrap_or_default();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak2.upgrade() else { return };
                    let ids = group_ids2.lock().unwrap();
                    let Some(idx) = ids.iter().position(|g| g == &group_hex) else {
                        return;
                    };
                    drop(ids);
                    let chats_messages = ui.get_chats_messages();
                    let overlay = pending_state.lock().unwrap();
                    rebuild_chat_messages_from(
                        &b,
                        &overlay,
                        &chats_messages,
                        idx,
                        &group_hex,
                        &msgs,
                    );
                });
            });
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
            // Localized dialog title comes from the Slint @tr catalogs (the
            // project keeps all i18n there); read it here on the UI thread,
            // then move it into the blocking dialog task.
            let dialog_title = ui
                .global::<NativeDialogStrings>()
                .get_choose_profile_picture()
                .to_string();
            let weak = ui.as_weak();
            let backend_cell = backend_cell.clone();
            tokio_handle.spawn(async move {
                let chosen = tokio::task::spawn_blocking(move || {
                    rfd::FileDialog::new()
                        .set_title(dialog_title)
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
}
