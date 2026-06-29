use crate::*;

/// Push the current account's avatar (initials + palette) onto the UI.
/// Drives the left-rail avatar tile and the outgoing-message sender avatar
/// so they reflect the user's profile instead of a stale default.
pub(crate) fn set_my_avatar(ui: &DarkMatterLinux, backend: &Backend) {
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
pub(crate) fn set_rail_badges(ui: &DarkMatterLinux, chats: &ModelRc<ChatMeta>) {
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
pub(crate) fn clear_chat_unread_row(ui: &DarkMatterLinux, idx: usize) {
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
pub(crate) fn my_avatar_label(backend: &Backend, my_id: &str) -> String {
    let name = backend.account_display_name(my_id);
    if name.is_empty() || name == "You" {
        my_id.to_string()
    } else {
        name
    }
}

/// One-time encryption-banner entrance when a chat is opened for the first time.
pub(crate) fn trigger_encryption_banner_entrance(
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
pub(crate) fn boot_phase_for_status(status: &str) -> i32 {
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
pub(crate) fn local_animal_avatar_image(npub: &str, name: &str) -> Option<slint::Image> {
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
pub(crate) fn random_profile_name() -> String {
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
pub(crate) fn publish_random_profile_async(
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
pub(crate) fn seed_profile_picture(
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

pub(crate) fn apply_profile(ui: &DarkMatterLinux, profile: Option<&UserProfileMetadata>) {
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

pub(crate) fn profile_from_ui(ui: &DarkMatterLinux) -> UserProfileMetadata {
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
pub(crate) fn replace_message_row(
    outer: &ModelRc<ModelRc<ChatMessage>>,
    idx: usize,
    msgs: Vec<ChatMessage>,
) {
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
pub(crate) fn refresh_stamps_everywhere(
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
pub(crate) struct ChatListSnapshot {
    pub(crate) records: Vec<AppGroupRecord>,
    /// Parallel to `records`.
    pub(crate) latest: Vec<Option<AppMessageRecord>>,
    /// Parallel to `records`: per-chat unread count at snapshot time.
    pub(crate) unread: Vec<u32>,
    pub(crate) first_msgs: Vec<AppMessageRecord>,
}

pub(crate) fn fetch_chat_list_snapshot(backend: &Backend) -> Option<ChatListSnapshot> {
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
pub(crate) fn refresh_chats_async(
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
pub(crate) fn refresh_all_chat_models_async(
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

pub(crate) fn refresh_chats_from(
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
        .map(|((r, latest), &unread)| chat_meta_from(r, latest.as_ref(), &my_id, backend, unread))
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
        let deletes = aggregate_deletes(msgs);
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
                let deleted = deletes.contains(&m.message_id_hex);
                chat_message_from_with_reactions(
                    m, &by_id, &my_id, &my_label, r, e, deleted, &profiles, is_group, false,
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
pub(crate) fn merge_chat_list_rows_async(
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

pub(crate) fn merge_chat_list_rows_from(
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
pub(crate) fn populate_models_for_active(
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
pub(crate) fn refresh_accounts_model(ui: &DarkMatterLinux, backend: &Arc<Backend>) {
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
pub(crate) fn vault_set_async(vault: &Arc<Mutex<Vault>>, key: String, value: String) {
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
pub(crate) fn import_backup_error(e: &backup::BackupError) -> String {
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
pub(crate) fn merge_imported_accounts(
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
pub(crate) const NOTIF_SKEW_SECS: u64 = 120;

pub(crate) fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Cap on how many recent messages a per-chat unread recount scans. Counts
/// above this saturate (the badge shows `99+` long before, anyway), so the
/// scan stays cheap even for a chat with a deep backlog.
pub(crate) const UNREAD_SCAN_CAP: usize = 200;

/// Process-wide unread state, lazily initialized from the persisted
/// `Settings::last_read` markers on first use. A `OnceLock` singleton (like
/// `active_group_slot`) rather than a threaded handle, because the chat watcher
/// and the chat-list snapshot fetch both run off the UI thread and would
/// otherwise need it plumbed through every refresh path.
pub(crate) fn unread_state() -> &'static unread::UnreadState {
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
pub(crate) fn count_unread(
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
pub(crate) fn refresh_unread_chrome(ui: &DarkMatterLinux) {
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
pub(crate) fn notification_body(
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

pub(crate) fn install_chat_watcher(
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
pub(crate) fn fallback_chat_meta(record: &AppGroupRecord) -> ChatMeta {
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
