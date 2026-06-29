use crate::*;

pub(crate) fn chat_meta_from(
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
pub(crate) fn is_visible_chat_message(record: &AppMessageRecord) -> bool {
    // Strict allow-list: only the chat kind. Reactions/deletes/streams/etc.
    // need their own renderers; until they have one, hide them rather than
    // dump raw JSON into the chat scroll.
    if record.kind != 9 {
        return false;
    }
    // "Delete for me" — locally hidden, never rendered (the message stays on
    // the wire for everyone else). Checked here so every render path (full
    // rebuild, grouping keys, live append) honours it uniformly.
    if is_hidden_message(&record.message_id_hex) {
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
pub(crate) fn chat_message_from_with_reactions(
    record: &AppMessageRecord,
    records_by_id: &HashMap<&str, &AppMessageRecord>,
    my_account_id_hex: &str,
    my_label: &str,
    reactions: Vec<Reaction>,
    edit: Option<EditState>,
    // True when this message has been retracted for everyone (resolved kind-5 or
    // optimistic overlay). The row renders as a muted tombstone.
    deleted: bool,
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
        // A tombstone carries no body, reactions, attachments, or affordances —
        // the bubble swaps in the "this message was deleted" placeholder and the
        // row hides its toolbar/reaction chips. We still null out the model
        // fields so nothing leaks (e.g. a reaction chip row, an edit badge).
        text: if deleted { s("") } else { s(display_text) },
        lines: if deleted {
            ModelRc::new(VecModel::from(Vec::<MessageLine>::new()))
        } else {
            lines
        },
        jumbo_emoji: !deleted && jumbo_emoji,
        deleted,
        stamp: s(&format_unix(record.recorded_at)),
        outgoing,
        edited: !deleted && edited,
        edit_count: if deleted { 0 } else { edit_count },
        can_edit: outgoing && !deleted,
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
        reactions: ModelRc::new(VecModel::from(if deleted { Vec::new() } else { reactions })),
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
        effect_id: if deleted { 0 } else { effect_id },
        effect_clip_x,
        effect_clip_y,
        effect_autoplay: !deleted && effect_autoplay,
    }
}

/// Resolve a record's reply target into (parent_id, author_label, preview).
/// Returns empty strings when the record isn't a reply. The author label is
/// "You" for your own messages and the parent's avatar-initials otherwise —
/// matches what the bubble's quoted-block expects to render.
pub(crate) fn reply_preview_for(
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
pub(crate) fn truncate_preview(text: &str, max: usize) -> String {
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
pub(crate) fn pending_chat_message(
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
        // A pending row is freshly composed; it can't already be retracted.
        deleted: false,
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
pub(crate) fn apply_reaction_overlay(
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
pub(crate) fn apply_reaction_to_model_row(
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
pub(crate) fn apply_edit_to_model_row(
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
pub(crate) fn refresh_one_message_row_from(
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
pub(crate) fn refresh_one_message_row_async(
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
pub(crate) fn with_inner_messages<R>(
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
pub(crate) fn find_message_row(vm: &VecModel<ChatMessage>, id: &str) -> Option<usize> {
    (0..vm.row_count()).find(|&i| {
        vm.row_data(i)
            .map(|r| r.message_id.as_str() == id)
            .unwrap_or(false)
    })
}

/// Build the `ChatMessage` for a single record, applying any pending-reaction
/// overlay so optimistic chips show up the moment the user clicks them.
pub(crate) fn build_one_message_row(
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
    let mut deletes = aggregate_deletes(all_records);
    apply_delete_overlay(&mut deletes, group_hex, overlay);
    let deleted = deletes.contains(&record.message_id_hex);
    // Resolve just this record's sender (single-row refresh path).
    let profiles = build_sender_profiles(backend, std::slice::from_ref(record), my_id);
    let is_group = backend.group_member_count(group_hex) > 2;
    let by_id: HashMap<&str, &AppMessageRecord> = all_records
        .iter()
        .map(|m| (m.message_id_hex.as_str(), m))
        .collect();
    chat_message_from_with_reactions(
        record, &by_id, my_id, my_label, r, e, deleted, &profiles, is_group, true,
    )
}

/// Rebuild one chat's message row from `(backend snapshot ∪ pending overlay)`.
/// This is the single source of truth — every code path that mutates state
/// (send, react, unreact, watcher fires) ends here.
/// Consecutive messages from the same sender within this many seconds collapse
/// into one visual group: a single trailing avatar, one name label, tightened
/// corners, and no inter-bubble gap.
pub(crate) const GROUP_WINDOW_SECS: u64 = 5 * 60;

/// A grouping key: (sender_lowercased, is_outgoing, recorded_at_secs).
pub(crate) type GroupKey = (String, bool, u64);

pub(crate) fn keys_grouped(a: &GroupKey, b: &GroupKey) -> bool {
    a.1 == b.1 && a.0 == b.0 && a.2.abs_diff(b.2) <= GROUP_WINDOW_SECS
}

/// Stamp first/last/avatar/name/gap grouping flags onto a freshly-built run of
/// rows. `keys` must be in the same order and length as `rows`.
pub(crate) fn apply_grouping(rows: &mut [ChatMessage], keys: &[GroupKey]) {
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
pub(crate) fn grouping_keys(
    msgs: &[AppMessageRecord],
    my_id: &str,
    pending_count: usize,
) -> Vec<GroupKey> {
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
pub(crate) fn push_message_grouped(vm: &VecModel<ChatMessage>, mut row: ChatMessage) {
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
pub(crate) fn preserve_grouping_flags(
    vm: &VecModel<ChatMessage>,
    pos: usize,
    row: &mut ChatMessage,
) {
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
pub(crate) fn rebuild_chat_messages_from(
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
    let mut deletes = aggregate_deletes(msgs);
    apply_delete_overlay(&mut deletes, group_hex, pending);
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
            let deleted = deletes.contains(&m.message_id_hex);
            chat_message_from_with_reactions(
                m, &by_id, &my_id, &my_label, r, e, deleted, &profiles, is_group, false,
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
pub(crate) fn aggregate_reactions(
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
pub(crate) struct EditState {
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
pub(crate) fn aggregate_edits(
    records: &[AppMessageRecord],
) -> std::collections::HashMap<String, EditState> {
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

/// Walk all records and resolve kind-5 "delete for everyone" retractions into
/// the set of target message ids that should render as a tombstone.
///
/// Authorship is enforced exactly like [`aggregate_edits`]: a delete is only
/// honored when its authenticated author matches the *original* message's
/// author. A kind-5 referencing someone else's message is ignored (you can't
/// retract a message you didn't send).
pub(crate) fn aggregate_deletes(records: &[AppMessageRecord]) -> std::collections::HashSet<String> {
    use std::collections::{HashMap, HashSet};
    // message_id → original author, for kind-9 chat messages only.
    let mut author_of: HashMap<&str, &str> = HashMap::new();
    for r in records {
        if r.kind == 9 {
            author_of.insert(r.message_id_hex.as_str(), r.sender.as_str());
        }
    }
    let mut deleted: HashSet<String> = HashSet::new();
    for r in records {
        if r.kind != 5 {
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
        let Some(orig_author) = author_of.get(target) else {
            continue;
        };
        if !r.sender.eq_ignore_ascii_case(orig_author) {
            continue;
        }
        deleted.insert(target.to_string());
    }
    deleted
}

/// Layer the pending-delete overlay onto an aggregated delete set, so an
/// optimistic "delete for everyone" tombstones the row before its kind-5 echoes
/// back. Mirrors [`apply_edit_overlay`].
pub(crate) fn apply_delete_overlay(
    deleted: &mut std::collections::HashSet<String>,
    group_hex: &str,
    overlay: &PendingState,
) {
    for (g, target) in &overlay.deletes {
        if g == group_hex {
            deleted.insert(target.clone());
        }
    }
}

/// Layer the pending-edit overlay onto an aggregated edit map, so an
/// optimistic edit shows before its kind-1009 echoes back. Mirrors
/// [`apply_reaction_overlay`].
pub(crate) fn apply_edit_overlay(
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
pub(crate) fn build_edit_history(
    records: &[AppMessageRecord],
    message_id: &str,
) -> Vec<EditVersion> {
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
pub(crate) fn qr_image(text: &str) -> slint::Image {
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
