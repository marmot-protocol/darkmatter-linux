use crate::*;

// ─── Forward a message to another chat ──────────────────────────────────────
//
// The message context menu's "Forward" item stashes the source message id on
// the root (`forward-src-id`) and opens the chat picker. Picking a destination
// fires `request-forward(dest_idx)`, handled here:
//
//   1. Resolve the source + destination group hexes (source is the still-active
//      chat behind the picker; destination is the picked index).
//   2. Switch the UI to the destination chat so the re-sent content is visible.
//   3. Off the UI thread, read the source record. Forward its body text as a
//      fresh send, and re-encrypt each attachment for the destination group by
//      downloading+decrypting from the source and re-uploading to the target.
//
// Forwarding never carries the original's reply context, reactions, or effect —
// it re-sends content only, exactly like composing it anew in the target chat.
pub(crate) fn wire_forward(ui: &DarkMatterLinux, cx: &Cx) {
    let Cx {
        backend_cell,
        group_ids,
        pending_state,
        vault_cell,
        ..
    } = cx.clone();

    ui.on_request_forward({
        let weak = ui.as_weak();
        move |dest_idx| {
            let Some(ui) = weak.upgrade() else { return };
            let dest_idx = dest_idx as usize;
            let src_id = ui.get_forward_src_id().to_string();
            // A still-optimistic (unconfirmed) message has no backend record to
            // forward from; ignore until it lands.
            if src_id.is_empty() || src_id.starts_with("pending:") {
                return;
            }
            let src_idx = ui.get_active_chat() as usize;
            let (src_group, dest_group) = {
                let ids = group_ids.lock().unwrap();
                match (ids.get(src_idx).cloned(), ids.get(dest_idx).cloned()) {
                    (Some(s), Some(d)) => (s, d),
                    _ => return,
                }
            };
            // Forwarding into the same thread is a no-op (the picker hides the
            // current chat, but guard anyway).
            if src_group == dest_group {
                return;
            }
            let Some(backend) = backend_cell.lock().unwrap().clone() else {
                ui.set_backend_error(error_copy().not_connected.into());
                return;
            };

            // Land the user in the destination so they watch the forward arrive.
            // The optimistic bubbles below reconcile by group hex, so they land
            // in the right chat regardless of this switch's async rebuild.
            ui.invoke_chat_selected(dest_idx as i32);

            let weak = weak.clone();
            let backend_cell = backend_cell.clone();
            let group_ids = group_ids.clone();
            let pending_state = pending_state.clone();
            let vault_cell = vault_cell.clone();
            let b = backend.clone();
            backend.tokio_handle().spawn(async move {
                // Resolve the source record (sqlite read) off the UI thread.
                let all = b
                    .messages(&src_group, Some(msg_window_for(&src_group)))
                    .unwrap_or_default();
                let Some(rec) = all.iter().find(|m| m.message_id_hex == src_id).cloned() else {
                    return;
                };
                let body = rec.plaintext.trim().to_string();
                let refs = parse_all_media_references(&rec.tags, rec.source_epoch);

                // Plain text (no attachments) → one fresh send.
                if refs.is_empty() {
                    if !body.is_empty() {
                        spawn_text_forward(
                            weak,
                            backend_cell,
                            group_ids,
                            pending_state,
                            vault_cell,
                            dest_group,
                            body,
                        );
                    }
                    return;
                }

                forward_media(
                    weak,
                    backend_cell,
                    group_ids,
                    pending_state,
                    vault_cell,
                    b,
                    src_group,
                    dest_group,
                    body,
                    refs,
                );
            });
        }
    });
}

/// Download+decrypt every attachment on the source message, then re-upload them
/// (re-encrypted for the destination group) mirroring the composer's staged
/// flush: 2+ images become one album bubble, a lone image or any other file
/// goes out on its own. Downloads run concurrently; a shared gather fires the
/// flush once the last one resolves. The body text (a caption) is forwarded
/// first so it reads above the media.
#[allow(clippy::too_many_arguments)]
fn forward_media(
    weak: slint::Weak<DarkMatterLinux>,
    backend_cell: BackendCell,
    group_ids: Arc<Mutex<Vec<String>>>,
    pending_state: Arc<Mutex<PendingState>>,
    vault_cell: VaultCell,
    backend: Arc<Backend>,
    src_group: String,
    dest_group: String,
    body: String,
    refs: Vec<MediaAttachmentReference>,
) {
    if !body.is_empty() {
        spawn_text_forward(
            weak.clone(),
            backend_cell.clone(),
            group_ids.clone(),
            pending_state.clone(),
            vault_cell.clone(),
            dest_group.clone(),
            body,
        );
    }

    let n = refs.len();
    // Order-preserving slots so a multi-image album keeps its original order.
    let slots: Arc<Mutex<Vec<Option<StagedFile>>>> =
        Arc::new(Mutex::new((0..n).map(|_| None).collect()));
    let remaining = Arc::new(AtomicUsize::new(n));

    for (i, reference) in refs.into_iter().enumerate() {
        let slots = slots.clone();
        let remaining = remaining.clone();
        let weak = weak.clone();
        let backend_cell = backend_cell.clone();
        let group_ids = group_ids.clone();
        let pending_state = pending_state.clone();
        let vault_cell = vault_cell.clone();
        let dest_group = dest_group.clone();
        backend.download_media_async(&src_group, reference, move |result| {
            match result {
                Ok(dl) => {
                    let is_image = mime_is_image(&dl.media_type);
                    slots.lock().unwrap()[i] = Some(StagedFile {
                        file_name: dl.file_name,
                        media_type: dl.media_type,
                        bytes: dl.plaintext,
                        is_image,
                        preview: None,
                        thumb: None,
                    });
                }
                Err(e) => {
                    eprintln!("[forward] attachment {i} download failed: {e:#}");
                }
            }
            // Last download in → flush the collected attachments to the target.
            if remaining.fetch_sub(1, AtomicOrdering::SeqCst) == 1 {
                let files: Vec<StagedFile> = std::mem::take(&mut *slots.lock().unwrap())
                    .into_iter()
                    .flatten()
                    .collect();
                flush_forwarded_media(
                    weak,
                    backend_cell,
                    group_ids,
                    pending_state,
                    vault_cell,
                    dest_group,
                    files,
                );
            }
        });
    }
}

/// Route the decrypted, re-collected attachments into the destination exactly
/// like `on_send_message`'s staged flush: images chunk into albums of 10, a
/// single leftover image or any non-image file sends on its own. Each spawn
/// builds its own optimistic bubble and reconciles by group hex.
fn flush_forwarded_media(
    weak: slint::Weak<DarkMatterLinux>,
    backend_cell: BackendCell,
    group_ids: Arc<Mutex<Vec<String>>>,
    pending_state: Arc<Mutex<PendingState>>,
    vault_cell: VaultCell,
    dest_group: String,
    files: Vec<StagedFile>,
) {
    let (images, others): (Vec<StagedFile>, Vec<StagedFile>) =
        files.into_iter().partition(|f| f.is_image);
    for chunk in images.chunks(10) {
        if chunk.len() == 1 {
            let f = chunk[0].clone();
            spawn_attachment_send(
                weak.clone(),
                backend_cell.clone(),
                group_ids.clone(),
                pending_state.clone(),
                vault_cell.clone(),
                dest_group.clone(),
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
                dest_group.clone(),
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
            dest_group.clone(),
            f.file_name,
            f.media_type,
            f.bytes,
            f.is_image,
            f.preview,
            None,
        );
    }
}

/// Text analog of [`spawn_attachment_send`]: insert an optimistic bubble in the
/// destination chat, durably queue the send, dispatch it in the background, and
/// reconcile the row by group hex on ack (or flip it red on an online failure /
/// leave it pending while offline). Used only by the forward flow — the normal
/// composer send owns its own optimistic block inline.
pub(crate) fn spawn_text_forward(
    weak: slint::Weak<DarkMatterLinux>,
    backend_cell: BackendCell,
    group_ids: Arc<Mutex<Vec<String>>>,
    pending_state: Arc<Mutex<PendingState>>,
    vault_cell: VaultCell,
    group_hex: String,
    text: String,
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
        let Some(backend) = backend_cell.lock().unwrap().clone() else {
            return;
        };

        let temp_id = next_temp_id();
        let send = PendingSend {
            temp_id: temp_id.clone(),
            text: text.clone(),
            failed: false,
            reply_to: None,
            media: Vec::new(),
            effect: 0,
        };
        {
            let mut overlay = pending_state.lock().unwrap();
            overlay.add_send(&group_hex, send.clone());
        }
        let my_id = backend.account().account_id_hex.clone();
        let my_label = my_avatar_label(&backend, &my_id);
        offline_persist(
            &vault_cell,
            &offline_queue::QueuedSend {
                temp_id: temp_id.clone(),
                account_id_hex: my_id.clone(),
                group_hex: group_hex.clone(),
                kind: offline_queue::QueuedKind::Text {
                    text: text.clone(),
                    reply_to: None,
                    effect: 0,
                },
                enqueued_at: offline_queue::now_secs(),
            },
        );
        let pending_row = pending_chat_message(&send, &my_id, &my_label);
        with_inner_messages(&chats_messages, idx, |vm| {
            push_message_grouped(vm, pending_row)
        });
        ui.set_messages_scroll_tick(ui.get_messages_scroll_tick() + 1);
        offline_inflight_insert(&temp_id);

        // Dispatch + reconcile by group hex (mirrors main.rs `dispatch_send`).
        let weak_cb = weak.clone();
        let group_ids_cb = group_ids.clone();
        let pending_state_cb = pending_state.clone();
        let backend_cell_cb = backend_cell.clone();
        let group_hex_cb = group_hex.clone();
        let temp_id_cb = temp_id.clone();
        let on_done = move |result: anyhow::Result<marmot_app::SendSummary>| {
            let weak = weak_cb.clone();
            let group_ids = group_ids_cb.clone();
            let pending_state = pending_state_cb.clone();
            let backend_cell = backend_cell_cb.clone();
            let group_hex = group_hex_cb.clone();
            let temp_id = temp_id_cb.clone();
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
                let ids = group_ids.lock().unwrap();
                let Some(idx) = ids.iter().position(|g| g == &group_hex) else {
                    return;
                };
                drop(ids);
                let chats_messages = ui.get_chats_messages();
                match result {
                    Ok(summary) => {
                        let real_id = summary.message_ids.first().cloned();
                        pending_state
                            .lock()
                            .unwrap()
                            .drop_send(&group_hex, &temp_id);
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
                                let rec = all.iter().find(|m| m.message_id_hex == id).cloned()?;
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
                        eprintln!("[forward] {e:#}");
                        if !online {
                            // Offline: leave the bubble pending + queued for flush.
                            return;
                        }
                        ui.set_backend_error(friendly_error("forward", &e).into());
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
        backend.send_text_async(&group_hex, &text, Vec::new(), on_done);
    });
}
