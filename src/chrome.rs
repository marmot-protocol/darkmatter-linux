use crate::*;

// ─── User-selectable stamp formats ──────────────────────────────────────
// Mirrors `Settings::{time_format,date_format}` as process-wide atomics so
// the formatters (called per row in rebuild loops) never touch disk. Synced
// at boot and whenever the user changes the pickers in Settings → General.
pub(crate) static TIME_FORMAT_12H: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// 0 = mdy ("Jun 12"), 1 = dmy ("12 Jun"), 2 = iso ("2026-06-12").
pub(crate) static DATE_FORMAT_KIND: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(0);

pub(crate) fn apply_stamp_formats(settings: &Settings) {
    use std::sync::atomic::Ordering;
    TIME_FORMAT_12H.store(settings.time_format == "12h", Ordering::Relaxed);
    let kind = match settings.date_format.as_str() {
        "dmy" => 1,
        "iso" => 2,
        _ => 0,
    };
    DATE_FORMAT_KIND.store(kind, Ordering::Relaxed);
}

/// Clock part of a stamp, honoring the 12h/24h preference.
pub(crate) fn format_clock(z: &jiff::Zoned) -> String {
    if TIME_FORMAT_12H.load(std::sync::atomic::Ordering::Relaxed) {
        let (h, half) = match z.hour() {
            0 => (12, "AM"),
            h @ 1..=11 => (h, "AM"),
            12 => (12, "PM"),
            h => (h - 12, "PM"),
        };
        format!("{}:{:02} {}", h, z.minute(), half)
    } else {
        format!("{:02}:{:02}", z.hour(), z.minute())
    }
}

/// Date part of a stamp, honoring the mdy/dmy/iso preference. `with_year`
/// is advisory for the named-month styles; ISO always carries the year.
pub(crate) fn format_date_part(z: &jiff::Zoned, with_year: bool) -> String {
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mi = (z.month() as usize).saturating_sub(1).min(11);
    match DATE_FORMAT_KIND.load(std::sync::atomic::Ordering::Relaxed) {
        1 => {
            if with_year {
                format!("{} {} {}", z.day(), months[mi], z.year())
            } else {
                format!("{} {}", z.day(), months[mi])
            }
        }
        2 => format!("{:04}-{:02}-{:02}", z.year(), z.month(), z.day()),
        _ => {
            if with_year {
                format!("{} {} {}", months[mi], z.day(), z.year())
            } else {
                format!("{} {}", months[mi], z.day())
            }
        }
    }
}

pub(crate) fn format_date_unix(secs: u64) -> String {
    if secs == 0 {
        return String::new();
    }
    let z = local_time(secs);
    format!("{} · {}", format_date_part(&z, false), format_clock(&z))
}

/// Render a unix-seconds timestamp as a clock stamp in the user's local
/// timezone, honoring the 12h/24h preference.
pub(crate) fn format_unix(secs: u64) -> String {
    let z = local_time(secs);
    format_clock(&z)
}

/// Friendly chat-list stamp: `HH:MM` for today, "Yesterday", the weekday
/// within the last week, `Mon DD` within the year, `Mon DD YYYY` beyond.
/// Date-granular on purpose — labels only go stale at midnight, so the
/// refresh is a once-a-day model rebuild instead of a per-minute tick.
/// (English like the month names in `format_date_unix`; gettext only covers
/// .slint strings today.)
pub(crate) fn format_chat_stamp(secs: u64) -> String {
    if secs == 0 {
        return String::new();
    }
    let z = local_time(secs);
    let now = jiff::Zoned::now();
    let days = z
        .date()
        .until(now.date())
        .map(|span| span.get_days())
        .unwrap_or(0);
    if days <= 0 {
        // Today — or a clock-skewed future stamp, which gets the same benefit
        // of the doubt rather than a nonsense date.
        return format_clock(&z);
    }
    if days == 1 {
        return "Yesterday".to_string();
    }
    if days < 7 {
        use jiff::civil::Weekday;
        return match z.weekday() {
            Weekday::Monday => "Mon",
            Weekday::Tuesday => "Tue",
            Weekday::Wednesday => "Wed",
            Weekday::Thursday => "Thu",
            Weekday::Friday => "Fri",
            Weekday::Saturday => "Sat",
            Weekday::Sunday => "Sun",
        }
        .to_string();
    }
    format_date_part(&z, z.year() != now.year())
}

/// Epoch seconds → civil time in the system timezone. Conversion happens
/// per-timestamp (not via a cached offset) so messages on either side of a
/// DST switch each get the offset that was in effect when they were sent.
pub(crate) fn local_time(secs: u64) -> jiff::Zoned {
    jiff::Timestamp::from_second(secs.min(i64::MAX as u64) as i64)
        .unwrap_or_default()
        .to_zoned(jiff::tz::TimeZone::system())
}

// ─── Contacts ──────────────────────────────────────────────────────────

/// Fetch the follow list AND the nickname map (a disk read — `Settings`
/// lives in a JSON file) on the backend runtime, then build + apply the
/// contact rows (+ avatar fetches) on the UI thread. `then` runs last, still
/// on the UI thread, for follow-ups that need the refreshed model (e.g.
/// selecting a freshly-added contact).
pub(crate) fn refresh_contacts_async(
    ui: &DarkMatterLinux,
    backend: &Arc<Backend>,
    then: impl FnOnce(&DarkMatterLinux) + Send + 'static,
) {
    let weak = ui.as_weak();
    let b = backend.clone();
    backend.tokio_handle().spawn(async move {
        let records = match b.follow_list() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[backend] follow_list failed: {e:#}");
                return;
            }
        };
        let nicknames = Settings::load().nicknames;
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            let contacts = ui.get_contacts();
            let rows: Vec<Contact> = records
                .iter()
                .map(|r| contact_from(r, &nicknames))
                .collect();
            if let Some(vm) = contacts.as_any().downcast_ref::<VecModel<Contact>>() {
                vm.set_vec(rows);
            }
            spawn_contact_avatar_fetches(&ui, &b);
            then(&ui);
        });
    });
}

/// Strip the `wss://`/`ws://` scheme and trailing slash from a relay URL for a
/// compact freshness label ("From relay.damus.io · 3h ago").
pub(crate) fn relay_host(url: &str) -> String {
    let h = url.trim();
    let h = h
        .strip_prefix("wss://")
        .or_else(|| h.strip_prefix("ws://"))
        .unwrap_or(h);
    h.trim_end_matches('/').to_string()
}

/// Coarse "N units ago" for a unix-seconds timestamp. English on purpose, like
/// the other Rust-side stamps (`format_chat_stamp`) — gettext only covers the
/// .slint catalogs today.
pub(crate) fn relative_since(secs: u64) -> String {
    if secs == 0 {
        return String::new();
    }
    let d = now_unix_secs().saturating_sub(secs);
    if d < 60 {
        "just now".to_string()
    } else if d < 3600 {
        format!("{}m ago", d / 60)
    } else if d < 86_400 {
        format!("{}h ago", d / 3600)
    } else {
        format!("{}d ago", d / 86_400)
    }
}

/// Build the contact-detail "Key package" row's (value, sublabel) from real
/// fetched metadata: the event's created-at + the relays it came from. Shared
/// by `contact_from` (directory cache) and the Refresh handler (live fetch) so
/// both render the same honest copy.
pub(crate) fn kp_labels(created_at: u64, source_relays: &[String]) -> (String, String) {
    let relay = source_relays
        .first()
        .map(|r| relay_host(r))
        .unwrap_or_default();
    let when = relative_since(created_at);
    let detail = match (relay.is_empty(), when.is_empty()) {
        (false, false) => format!("From {relay} · {when}"),
        (false, true) => format!("From {relay}"),
        (true, false) => format!("Published {when}"),
        (true, true) => "Published".to_string(),
    };
    ("Available".to_string(), detail)
}

pub(crate) fn contact_from(
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
    let display = if nickname.is_empty() {
        published.clone()
    } else {
        nickname.clone()
    };
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
    let picture_url = record.profile.as_ref().and_then(|p| p.picture.clone());
    let (picture, has_picture) = bind_cached_picture(picture_url.as_deref());
    // Real key-package state from the directory cache. Honest empty state when
    // the peer has none published yet — never the old hardcoded placeholder.
    let (kp_status, kp_detail) = match &record.key_package {
        Some(kp) => kp_labels(kp.created_at, &kp.source_relays),
        None => (
            "Not found".to_string(),
            "No key package on relays yet".to_string(),
        ),
    };
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
        picture,
        has_picture,
        kp_status: s(&kp_status),
        kp_detail: s(&kp_detail),
    }
}

/// Split the new-chat modal's members textarea into individual npubs/hex
/// pubkeys. Accepts whitespace, comma, semicolon, or newline as separators.
/// No validation — the marmot runtime parses each entry and errors out on
/// invalid input, which we surface back to the user.
pub(crate) fn parse_member_list(raw: &str) -> Vec<String> {
    raw.split(|c: char| c.is_whitespace() || c == ',' || c == ';')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

pub(crate) fn shorten_npub(npub: &str) -> String {
    if npub.len() <= 16 {
        return npub.to_string();
    }
    format!("{}…{}", &npub[..10], &npub[npub.len() - 6..])
}

// ─── Group members ─────────────────────────────────────────────────────

/// Process-wide record of which group is currently shown, so async group-avatar
/// decodes that finish after the user has switched chats don't paint the wrong
/// group's image into the header/panel.
pub(crate) fn active_group_slot() -> &'static Mutex<String> {
    use std::sync::OnceLock;
    static SLOT: OnceLock<Mutex<String>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(String::new()))
}

/// Push the admin group-settings surface (rename draft + group avatar) for the
/// active group. The avatar drives both the chat header and the members panel
/// via the `chat-group-*` root properties. For 1:1 chats the group avatar is
/// cleared (the header falls back to the peer avatar in `ChatMeta`).
pub(crate) fn push_group_settings_to_ui_from(
    ui: &DarkMatterLinux,
    backend: &Backend,
    group_hex: &str,
    rec: Option<&AppGroupRecord>,
    count: usize,
) {
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
pub(crate) fn spawn_group_image_fetch(
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

/// Everything the members panel needs from the backend, gathered OFF the UI
/// thread (`chats()` and `group_members()` hit sqlite, which can stall behind
/// sync writes or a slow disk).
pub(crate) struct MembersSnapshot {
    group_rec: Option<AppGroupRecord>,
    count: usize,
    viewer_is_admin: bool,
    admins: Vec<String>,
    members: Vec<AppGroupMemberRecord>,
}

pub(crate) fn fetch_members_snapshot(backend: &Backend, group_hex: &str) -> MembersSnapshot {
    let records = backend.chats().unwrap_or_default();
    let group_rec = records
        .iter()
        .find(|g| g.group_id_hex.eq_ignore_ascii_case(group_hex))
        .cloned();
    let admins = group_rec
        .as_ref()
        .map(|g| g.admin_policy.admins.clone())
        .unwrap_or_default();
    let me = &backend.account().account_id_hex;
    let viewer_is_admin = admins.iter().any(|a| a.eq_ignore_ascii_case(me));
    let members = backend.group_members(group_hex).unwrap_or_else(|e| {
        eprintln!("[members] {e:#}");
        Vec::new()
    });
    MembersSnapshot {
        count: backend.group_member_count(group_hex),
        group_rec,
        viewer_is_admin,
        admins,
        members,
    }
}

/// Fetch the members snapshot on the backend runtime, then apply it on the
/// UI thread. Marks `group_hex` as the active group slot immediately so a
/// stale completion (user already switched groups) is dropped at apply time.
pub(crate) fn push_group_members_to_ui_async(
    ui: &DarkMatterLinux,
    backend: &Arc<Backend>,
    group_hex: &str,
) {
    if let Ok(mut slot) = active_group_slot().lock() {
        *slot = group_hex.to_string();
    }
    let b = backend.clone();
    let group_hex = group_hex.to_string();
    let weak = ui.as_weak();
    backend.tokio_handle().spawn(async move {
        let snap = fetch_members_snapshot(&b, &group_hex);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            let still_active = active_group_slot()
                .lock()
                .map(|slot| slot.eq_ignore_ascii_case(&group_hex))
                .unwrap_or(false);
            if !still_active {
                return;
            }
            push_group_members_to_ui_from(&ui, &b, &group_hex, snap);
        });
    });
}

pub(crate) fn push_group_members_to_ui_from(
    ui: &DarkMatterLinux,
    backend: &Backend,
    group_hex: &str,
    snap: MembersSnapshot,
) {
    push_group_settings_to_ui_from(ui, backend, group_hex, snap.group_rec.as_ref(), snap.count);
    let count = snap.count;
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
    let viewer_is_admin = snap.viewer_is_admin;
    ui.set_chat_can_add_members(viewer_is_admin);
    let admins = snap.admins;
    let my_id = backend.account().account_id_hex.clone();
    // Build (row, picture_url) pairs so we can spawn per-member image fetches
    // after the initial paint. URLs are looked up once here and shipped to
    // the worker tasks; the model itself only carries the decoded image.
    let mut pairs: Vec<(GroupMember, Option<String>)> = snap
        .members
        .iter()
        .map(|m| group_member_from(backend, m, &my_id, &admins, viewer_is_admin))
        .collect();
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
pub(crate) fn spawn_message_avatar_fetches(
    ui: &DarkMatterLinux,
    backend: &Backend,
    msgs: &[AppMessageRecord],
) {
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
pub(crate) fn update_bubble_pictures(
    ui: &DarkMatterLinux,
    sender_id: &str,
    pixels: &PicturePixels,
) {
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
        let Some(mut row) = vm.row_data(i) else {
            continue;
        };
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
pub(crate) fn spawn_chat_list_avatar_fetches(ui: &DarkMatterLinux, backend: &Arc<Backend>) {
    // The enumeration itself reads `chats()` (sqlite) — runtime, not UI thread.
    let weak_outer = ui.as_weak();
    let b = backend.clone();
    backend.tokio_handle().spawn(async move {
        let chats = match b.chats() {
            Ok(c) => c,
            Err(_) => return,
        };
        for record in chats {
            let Some(peer) = b.direct_chat_peer(&record.group_id_hex) else {
                continue;
            };
            let (_, url) = b.account_name_and_picture(&peer);
            let Some(url) = url.map(|u| u.trim().to_string()).filter(|u| !u.is_empty()) else {
                continue;
            };
            if picture_cache_has(&url) {
                continue;
            }
            let npub = format!("mls:0x{}", short_hex(&record.group_id_hex));
            let weak = weak_outer.clone();
            b.tokio_handle().spawn(async move {
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
    });
}

/// Bind a decoded picture onto the chat-list row identified by `npub`.
pub(crate) fn update_chat_picture(ui: &DarkMatterLinux, npub: &str, pixels: &PicturePixels) {
    let chats = ui.get_chats();
    let Some(vm) = chats.as_any().downcast_ref::<VecModel<ChatMeta>>() else {
        return;
    };
    let img = rgba_to_slint_image(pixels);
    for i in 0..vm.row_count() {
        let Some(mut row) = vm.row_data(i) else {
            continue;
        };
        if row.npub.as_str() != npub {
            continue;
        }
        row.picture = img.clone();
        row.has_picture = true;
        vm.set_row_data(i, row);
        break;
    }
}

/// Queue async fetches for contact-list avatars whose picture URL isn't in
/// the cache yet. Mirrors [`spawn_chat_list_avatar_fetches`].
pub(crate) fn spawn_contact_avatar_fetches(ui: &DarkMatterLinux, backend: &Arc<Backend>) {
    // The enumeration itself reads `follow_list()` (sqlite) — runtime only.
    let weak_outer = ui.as_weak();
    let b = backend.clone();
    backend.tokio_handle().spawn(async move {
        let records = match b.follow_list() {
            Ok(v) => v,
            Err(_) => return,
        };
        for record in records {
            let Some(url) = record
                .profile
                .as_ref()
                .and_then(|p| p.picture.clone())
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty())
            else {
                continue;
            };
            if picture_cache_has(&url) {
                continue;
            }
            let account_id = record.account_id_hex.clone();
            let weak = weak_outer.clone();
            b.tokio_handle().spawn(async move {
                let Some(pixels) = fetch_picture_pixels(&url).await else {
                    return;
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        update_contact_picture(&ui, &account_id, &pixels);
                    }
                });
            });
        }
    });
}

/// Bind a decoded picture onto the contact row identified by `account_id`.
pub(crate) fn update_contact_picture(
    ui: &DarkMatterLinux,
    account_id: &str,
    pixels: &PicturePixels,
) {
    let contacts = ui.get_contacts();
    let Some(vm) = contacts.as_any().downcast_ref::<VecModel<Contact>>() else {
        return;
    };
    let img = rgba_to_slint_image(pixels);
    for i in 0..vm.row_count() {
        let Some(mut row) = vm.row_data(i) else {
            continue;
        };
        if !row.account_id.as_str().eq_ignore_ascii_case(account_id) {
            continue;
        }
        row.picture = img.clone();
        row.has_picture = true;
        vm.set_row_data(i, row);
        break;
    }
}

/// Queue async fetches for archived-chat avatars (1:1 peers only) whose
/// picture URL isn't in the cache yet.
pub(crate) fn spawn_archived_avatar_fetches(ui: &DarkMatterLinux, backend: &Arc<Backend>) {
    // The enumeration itself reads `archived_chats()` (sqlite) — runtime only.
    let weak_outer = ui.as_weak();
    let b = backend.clone();
    backend.tokio_handle().spawn(async move {
        let records = match b.archived_chats() {
            Ok(v) => v,
            Err(_) => return,
        };
        for record in records {
            let Some(peer) = b.direct_chat_peer(&record.group_id_hex) else {
                continue;
            };
            let Some(url) = b
                .account_picture_url(&peer)
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty())
            else {
                continue;
            };
            if picture_cache_has(&url) {
                continue;
            }
            let group_id = format!("mls:0x{}", short_hex(&record.group_id_hex));
            let weak = weak_outer.clone();
            b.tokio_handle().spawn(async move {
                let Some(pixels) = fetch_picture_pixels(&url).await else {
                    return;
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        update_archived_picture(&ui, &group_id, &pixels);
                    }
                });
            });
        }
    });
}

/// Bind a decoded picture onto the archived row identified by `group_id`.
pub(crate) fn update_archived_picture(
    ui: &DarkMatterLinux,
    group_id: &str,
    pixels: &PicturePixels,
) {
    let archived = ui.get_archived_chats();
    let Some(vm) = archived.as_any().downcast_ref::<VecModel<ArchivedChat>>() else {
        return;
    };
    let img = rgba_to_slint_image(pixels);
    for i in 0..vm.row_count() {
        let Some(mut row) = vm.row_data(i) else {
            continue;
        };
        if row.group_id.as_str() != group_id {
            continue;
        }
        row.picture = img.clone();
        row.has_picture = true;
        vm.set_row_data(i, row);
        break;
    }
}

pub(crate) fn spawn_member_picture_fetch(
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

pub(crate) fn update_member_picture(
    ui: &DarkMatterLinux,
    npub_short: &str,
    pixels: &PicturePixels,
) {
    let members = ui.get_chat_members();
    let Some(vm) = members.as_any().downcast_ref::<VecModel<GroupMember>>() else {
        return;
    };
    for i in 0..vm.row_count() {
        let Some(mut row) = vm.row_data(i) else {
            continue;
        };
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
pub(crate) async fn fetch_picture_pixels(url: &str) -> Option<PicturePixels> {
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

pub(crate) fn group_member_from(
    backend: &Backend,
    record: &AppGroupMemberRecord,
    my_account_id_hex: &str,
    admins: &[String],
    viewer_is_admin: bool,
) -> (GroupMember, Option<String>) {
    let is_self = record.member_id_hex.eq_ignore_ascii_case(my_account_id_hex);
    let is_admin = admins
        .iter()
        .any(|a| a.eq_ignore_ascii_case(&record.member_id_hex));
    // The viewer can hand out admin only to other members who aren't admins yet.
    let can_promote = viewer_is_admin && !is_self && !is_admin;
    // Demote another admin, or step down from one's own admin role.
    let can_demote = viewer_is_admin && is_admin && !is_self;
    let can_self_demote = viewer_is_admin && is_admin && is_self;
    let mut name = backend.account_display_name(&record.member_id_hex);
    if let Some(label) = record.account.as_ref().filter(|s| !s.is_empty())
        && name.starts_with("0x")
    {
        name = label.clone();
    }
    let npub =
        npub_for_account_id(&record.member_id_hex).unwrap_or_else(|_| record.member_id_hex.clone());
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
pub(crate) const MAX_AVATAR_DECODE_PX: u32 = 512;

/// Decode image bytes to RGBA, downscaling to [`MAX_AVATAR_DECODE_PX`].
pub(crate) fn decode_avatar_pixels(bytes: &[u8]) -> Result<PicturePixels, image::ImageError> {
    let img = image::load_from_memory(bytes)?;
    let img = if img.width() > MAX_AVATAR_DECODE_PX || img.height() > MAX_AVATAR_DECODE_PX {
        img.thumbnail(MAX_AVATAR_DECODE_PX, MAX_AVATAR_DECODE_PX)
    } else {
        img
    };
    let img = img.to_rgba8();
    let (w, h) = img.dimensions();
    Ok(PicturePixels {
        w,
        h,
        rgba: img.into_raw(),
    })
}

pub(crate) fn rgba_to_slint_image(pixels: &PicturePixels) -> slint::Image {
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        &pixels.rgba,
        pixels.w,
        pixels.h,
    );
    slint::Image::from_rgba8(buffer)
}

// ─── Archived ──────────────────────────────────────────────────────────

/// Archived-list state gathered OFF the UI thread (sqlite reads).
pub(crate) struct ArchivedSnapshot {
    records: Vec<AppGroupRecord>,
    /// Parallel to `records`.
    latest: Vec<Option<AppMessageRecord>>,
}

pub(crate) fn fetch_archived_snapshot(backend: &Backend) -> Option<ArchivedSnapshot> {
    let records = match backend.archived_chats() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[backend] archived_chats failed: {e:#}");
            return None;
        }
    };
    let latest = records
        .iter()
        .map(|r| backend.latest_message(&r.group_id_hex))
        .collect();
    Some(ArchivedSnapshot { records, latest })
}

pub(crate) fn refresh_archived_from(
    backend: &Backend,
    snap: &ArchivedSnapshot,
    archived: &ModelRc<ArchivedChat>,
    archived_group_ids: &Arc<Mutex<Vec<String>>>,
) {
    let my_id = backend.account().account_id_hex.clone();
    let mut ids = Vec::with_capacity(snap.records.len());
    let rows: Vec<ArchivedChat> = snap
        .records
        .iter()
        .zip(snap.latest.iter())
        .map(|(r, latest)| {
            ids.push(r.group_id_hex.clone());
            archived_from(r, latest.as_ref(), &my_id, backend)
        })
        .collect();
    *archived_group_ids.lock().unwrap() = ids;
    if let Some(vm) = archived.as_any().downcast_ref::<VecModel<ArchivedChat>>() {
        vm.set_vec(rows);
    }
}

/// Fetch + apply the archived list (+ avatar fetches) off the UI thread.
pub(crate) fn refresh_archived_async(
    ui: &DarkMatterLinux,
    backend: &Arc<Backend>,
    archived_group_ids: &Arc<Mutex<Vec<String>>>,
) {
    let weak = ui.as_weak();
    let b = backend.clone();
    let archived_group_ids = archived_group_ids.clone();
    backend.tokio_handle().spawn(async move {
        let Some(snap) = fetch_archived_snapshot(&b) else {
            return;
        };
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            let archived = ui.get_archived_chats();
            refresh_archived_from(&b, &snap, &archived, &archived_group_ids);
            spawn_archived_avatar_fetches(&ui, &b);
        });
    });
}

pub(crate) fn archived_from(
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
    // Archived 1:1 chats keep the peer's profile picture; groups stay on the
    // gradient (group images are an MLS component, not a public URL).
    let picture_url = backend
        .direct_chat_peer(&record.group_id_hex)
        .and_then(|peer| backend.account_picture_url(&peer));
    let (picture, has_picture) = bind_cached_picture(picture_url.as_deref());
    ArchivedChat {
        name: s(&name),
        last_msg: s(&last_msg),
        last_date: s(&last_date),
        av_a: a,
        av_b: b,
        av_initials: s(&init),
        members: backend.group_member_count(&record.group_id_hex) as i32,
        group_id: s(&format!("mls:0x{}", short_hex(&record.group_id_hex))),
        picture,
        has_picture,
    }
}

// ─── Per-chat live message watcher ─────────────────────────────────────

/// Attach a watcher that appends new messages into the inner messages model
/// for the currently-open chat. Caller is responsible for aborting the
/// returned `JoinHandle` when the user switches chats.
pub(crate) fn install_message_watcher(
    backend: &Backend,
    weak: Weak<DarkMatterLinux>,
    backend_cell: Arc<Mutex<Option<Arc<Backend>>>>,
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
        if !matches!(kind, 9 | 7 | 5 | 1009) {
            return;
        }
        let weak = weak.clone();
        let backend_cell = backend_cell.clone();
        let pending_state = pending_state.clone();
        let group_hex_inner = group_hex_for_filter.clone();
        let msg_id = received.message_id_hex.clone();
        let target_id_for_reaction: Option<String> = if kind == 7 || kind == 5 || kind == 1009 {
            received
                .tags
                .iter()
                .find(|t| t.len() >= 2 && t[0] == "e")
                .map(|t| t[1].clone())
        } else {
            None
        };
        // This callback runs on the backend runtime. Read the window snapshot
        // HERE — the event-loop closure below must never touch sqlite (it can
        // stall behind sync writes or a slow disk and freeze the UI).
        let Some(b) = backend_cell.lock().unwrap().clone() else {
            return;
        };
        let all = b
            .messages(&group_hex_inner, Some(msg_window_for(&group_hex_inner)))
            .unwrap_or_default();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            let overlay = pending_state.lock().unwrap();
            let chats_messages = ui.get_chats_messages();

            match kind {
                9 => {
                    // Chat message echo. If the row already exists (because
                    // we just reconciled our own send), do nothing. Otherwise
                    // append it surgically — no full rebuild.
                    let my_id = b.account().account_id_hex.clone();
                    let my_label = my_avatar_label(&b, &my_id);
                    let Some(rec) = all.iter().find(|m| m.message_id_hex == msg_id).cloned() else {
                        return;
                    };
                    let row = build_one_message_row(
                        &rec,
                        &all,
                        &my_id,
                        &my_label,
                        &group_hex_inner,
                        &overlay,
                        &b,
                    );
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
                        spawn_message_avatar_fetches(&ui, &b, &all);
                    }
                }
                7 | 5 | 1009 => {
                    // Reaction, delete, or edit — surgical refresh of the
                    // target row. For an edit the snapshot now carries the
                    // kind-1009, so the rebuilt row picks up the new text.
                    let Some(target) = target_id_for_reaction else {
                        return;
                    };
                    refresh_one_message_row_from(
                        &b,
                        &overlay,
                        &chats_messages,
                        chat_idx,
                        &group_hex_inner,
                        &target,
                        &all,
                    );
                }
                _ => {}
            }
        });
    })
}
