use crate::*;

pub(crate) fn wire_reply_target(ui: &DarkMatterLinux) {
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
}

pub(crate) fn wire_messaging(ui: &DarkMatterLinux, cx: &Cx, h: &Handlers) {
    let Cx {
        chats_messages,
        backend_cell,
        vault_cell,
        group_ids,
        pending_state,
        staged_files,
        ..
    } = cx.clone();
    let Handlers {
        dispatch_send,
        edit_op,
        ..
    } = h.clone();
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

    // ─── Edit dispatch (optimistic, surgical) ─────────────────────────
    //
    // Same shape as `react_op`: stamp the overlay, rewrite ONLY the target
    // bubble's text locally, publish the kind-1009 in the background, then on
    // ack drop the overlay and refresh ONLY that row from the snapshot (which
    // now carries the confirmed edit). On failure the overlay is dropped too,
    // so the row reverts to its last confirmed text.

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
}

// Called by `on_send_message` for each staged attachment: insert the
// optimistic pending bubble, run the encrypted Blossom upload + kind-9
// publish, reconcile the bubble when the round-trip resolves.
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
pub(crate) fn spawn_attachment_send(
    weak: slint::Weak<DarkMatterLinux>,
    backend_cell: BackendCell,
    group_ids: Arc<Mutex<Vec<String>>>,
    pending_state: Arc<Mutex<PendingState>>,
    vault_cell: VaultCell,
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
                            if let (Some(id), Some(p)) = (real_id.as_ref(), local_preview.as_ref())
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
pub(crate) fn spawn_album_send(
    weak: slint::Weak<DarkMatterLinux>,
    backend_cell: BackendCell,
    group_ids: Arc<Mutex<Vec<String>>>,
    pending_state: Arc<Mutex<PendingState>>,
    vault_cell: VaultCell,
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
                                let rec = all.iter().find(|m| m.message_id_hex == id).cloned()?;
                                let overlay = pending_state.lock().unwrap();
                                let my_id = backend.account().account_id_hex.clone();
                                let my_label = my_avatar_label(backend, &my_id);
                                Some(build_one_message_row(
                                    &rec, &all, &my_id, &my_label, &group_hex, &overlay, backend,
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
