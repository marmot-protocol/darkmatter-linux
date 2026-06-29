use crate::*;

/// Read the profile from the directory cache on the backend runtime (a
/// sqlite read), then apply it on the UI thread.
pub(crate) fn populate_profile_async(ui: &DarkMatterLinux, backend: &Arc<Backend>) {
    let weak = ui.as_weak();
    let b = backend.clone();
    backend.tokio_handle().spawn(async move {
        let profile = b.load_profile();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            populate_profile_from(&b, &ui, profile);
        });
    });
}

pub(crate) fn populate_profile_from(
    backend: &Backend,
    ui: &DarkMatterLinux,
    profile: anyhow::Result<Option<UserProfileMetadata>>,
) {
    let picture_url = match profile {
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
pub(crate) fn fetch_profile_picture(ui: &DarkMatterLinux, backend: &Backend, url: &str) {
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
pub(crate) fn nostr_ref_to_hex(reference: &str) -> Option<String> {
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
pub(crate) fn open_profile_modal(
    ui: &DarkMatterLinux,
    backend_cell: &Arc<Mutex<Option<Arc<Backend>>>>,
    account_id_hex: &str,
) {
    let guard = backend_cell.lock().unwrap();
    let Some(backend) = guard.as_ref() else {
        return;
    };
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

    // Paint the loading skeleton immediately; follow-list membership and the
    // cached profile are sqlite reads, so they resolve on the runtime and
    // land a beat later (guarded against the modal moving on).
    ui.set_peer_profile_is_contact(false);
    ui.set_peer_profile_loading(true);
    apply_peer_profile(ui, backend, &id, &npub_short, None);
    ui.set_peer_profile_open(true);

    let weak = ui.as_weak();
    let backend_cell = backend_cell.clone();
    let b = backend.clone();
    let npub_short = npub_short.clone();
    backend.tokio_handle().spawn(async move {
        let is_contact = !is_self
            && b.follow_list()
                .map(|l| l.iter().any(|r| r.account_id_hex.eq_ignore_ascii_case(&id)))
                .unwrap_or(false);
        let cached = b.cached_profile(&id);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            // Stale guard: the modal may have closed or moved on to a
            // different user while the lookup was in flight.
            if !ui
                .get_peer_profile_account_id()
                .as_str()
                .eq_ignore_ascii_case(&id)
            {
                return;
            }
            ui.set_peer_profile_is_contact(is_contact);
            if let Some(profile) = cached {
                ui.set_peer_profile_loading(false);
                apply_peer_profile(&ui, &b, &id, &npub_short, Some(&profile));
                return;
            }
            let weak = ui.as_weak();
            let id_done = id.clone();
            let npub_short_done = npub_short.clone();
            b.fetch_profile_async(&id, move |profile| {
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    // Same stale guard for the relay round-trip.
                    if !ui
                        .get_peer_profile_account_id()
                        .as_str()
                        .eq_ignore_ascii_case(&id_done)
                    {
                        return;
                    }
                    ui.set_peer_profile_loading(false);
                    let guard = backend_cell.lock().unwrap();
                    let Some(backend) = guard.as_ref() else {
                        return;
                    };
                    match profile {
                        Some(p) => {
                            apply_peer_profile(&ui, backend, &id_done, &npub_short_done, Some(&p))
                        }
                        None => ui.set_peer_profile_not_found(true),
                    }
                });
            });
        });
    });
}

/// Push a resolved (or placeholder) profile into the modal's properties and
/// kick off the avatar download when a picture URL is present.
pub(crate) fn apply_peer_profile(
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
pub(crate) fn fetch_peer_profile_picture(
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
pub(crate) struct PicturePixels {
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) rgba: Vec<u8>,
}

/// Bind the user's own avatar picture by cache key (URL). Uses the shared
/// thread-local `Image` handle and SKIPS the property writes when the handle
/// is already bound: `my-av-picture` feeds the left-rail avatar AND every
/// outgoing bubble, so a fresh handle (or even a redundant set) re-renders
/// the whole conversation — the visible blink reported after background
/// syncs.
pub(crate) fn apply_picture(ui: &DarkMatterLinux, url: &str) {
    let Some(img) = cached_picture_image(url) else {
        return;
    };
    if ui.get_my_av_has_picture() && ui.get_my_av_picture() == img {
        return;
    }
    ui.set_my_av_picture(img);
    ui.set_my_av_has_picture(true);
}

pub(crate) fn picture_cache() -> &'static Mutex<HashMap<String, PicturePixels>> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Mutex<HashMap<String, PicturePixels>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn picture_cache_get(url: &str) -> Option<PicturePixels> {
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
pub(crate) fn cached_picture_image(url: &str) -> Option<slint::Image> {
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
pub(crate) fn cached_attachment_image(id: &str) -> Option<slint::Image> {
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
pub(crate) fn bind_cached_picture(url: Option<&str>) -> (slint::Image, bool) {
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
pub(crate) type SenderProfiles = std::collections::HashMap<String, (String, Option<String>)>;

pub(crate) fn build_sender_profiles(
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
pub(crate) const MESSAGE_WINDOW: usize = 80;

/// Per-chat message-window overrides (group_id_hex → record limit). Only
/// chats expanded via "Load earlier messages" have an entry; everything else
/// uses [`MESSAGE_WINDOW`]. Process-wide like the picture caches so the many
/// callback closures don't all need another captured handle.
pub(crate) fn msg_windows() -> &'static Mutex<HashMap<String, usize>> {
    use std::sync::OnceLock;
    static MAP: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Current record limit for a chat (default [`MESSAGE_WINDOW`]).
pub(crate) fn msg_window_for(group_hex: &str) -> usize {
    msg_windows()
        .lock()
        .ok()
        .and_then(|m| m.get(group_hex).copied())
        .unwrap_or(MESSAGE_WINDOW)
}

/// Grow a chat's window by one [`MESSAGE_WINDOW`] step; returns the new limit.
pub(crate) fn msg_window_expand(group_hex: &str) -> usize {
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
pub(crate) fn msg_window_reset(group_hex: &str) {
    if let Ok(mut m) = msg_windows().lock() {
        m.remove(group_hex);
    }
}

pub(crate) fn picture_cache_put(url: String, pixels: PicturePixels) {
    if let Ok(mut c) = picture_cache().lock() {
        c.insert(url, pixels);
    }
}

/// Presence check that doesn't clone the pixel buffer out of the cache —
/// `picture_cache_get(url).is_some()` copies the whole RGBA blob just to
/// throw it away.
pub(crate) fn picture_cache_has(url: &str) -> bool {
    picture_cache()
        .lock()
        .map(|c| c.contains_key(url))
        .unwrap_or(false)
}

/// Cache for decrypted+decoded image attachments. Keyed by the inner-event
/// message id so the same bubble can be rebuilt many times (overlay/reaction
/// changes) without losing the loaded image. Populated lazily on the first
/// tap of an image attachment.
pub(crate) fn attachment_image_cache() -> &'static Mutex<HashMap<String, PicturePixels>> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Mutex<HashMap<String, PicturePixels>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn attachment_image_cache_get(id: &str) -> Option<PicturePixels> {
    attachment_image_cache().lock().ok()?.get(id).cloned()
}

pub(crate) fn attachment_image_cache_put(id: String, pixels: PicturePixels) {
    if let Ok(mut c) = attachment_image_cache().lock() {
        c.insert(id, pixels);
    }
}

/// Tracks attachments currently being decrypted (so the UI shows "decrypting…"
/// and so we don't fire duplicate downloads on rapid clicks). Stores
/// message_id_hex while the round-trip is in flight.
pub(crate) fn attachment_in_flight() -> &'static Mutex<std::collections::HashSet<String>> {
    use std::sync::OnceLock;
    static SET: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Convert cached pixels into a Slint `Image`. Must be called on the UI thread —
/// `slint::Image` is `!Send` (it wraps a `VRc`).
pub(crate) fn image_from_pixels(pixels: &PicturePixels) -> slint::Image {
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        &pixels.rgba,
        pixels.w,
        pixels.h,
    );
    slint::Image::from_rgba8(buffer)
}

/// One image in the lightbox slideshow. `cache_key` is the shared attachment
/// cache handle (bare message id for a lone image, `id#index` for an album
/// member) — the same key the bubble renders from. `reference` is pre-resolved
/// so prev/next never re-reads sqlite to find what to download.
#[derive(Clone)]
pub(crate) struct ViewerItem {
    cache_key: String,
    reference: MediaAttachmentReference,
}

/// Every image in the chat window as an ordered slideshow list — one item per
/// image, expanding albums into their members. `cache_key` matches the render
/// path: a lone image keeps the bare message id; album members get `id#index`.
pub(crate) fn build_viewer_items(all: &[AppMessageRecord]) -> Vec<ViewerItem> {
    let mut items = Vec::new();
    for m in all {
        let imgs: Vec<MediaAttachmentReference> =
            parse_all_media_references(&m.tags, m.source_epoch)
                .into_iter()
                .filter(|r| mime_is_image(&r.media_type))
                .collect();
        if imgs.len() == 1 {
            items.push(ViewerItem {
                cache_key: m.message_id_hex.clone(),
                reference: imgs.into_iter().next().unwrap(),
            });
        } else {
            for (i, reference) in imgs.into_iter().enumerate() {
                items.push(ViewerItem {
                    cache_key: att_key(&m.message_id_hex, i),
                    reference,
                });
            }
        }
    }
    items
}

/// Ordered image attachments for the open lightbox + the current position.
/// UI-thread-only state (the lightbox and its callbacks all run there), held
/// in a thread-local so download-completion closures — which must be `Send`
/// to cross `invoke_from_event_loop` and so can't capture an `Rc` — can still
/// reach it once they hop back onto the event loop.
#[derive(Default)]
pub(crate) struct ViewerSlideshow {
    pub(crate) items: Vec<ViewerItem>,
    pub(crate) pos: usize,
}

thread_local! {
    pub(crate) static VIEWER_SLIDESHOW: std::cell::RefCell<ViewerSlideshow> =
        std::cell::RefCell::new(ViewerSlideshow::default());
}

/// Build the slideshow list for the open lightbox: every image attachment in
/// the chat window, in message order, with the tapped one selected. The
/// sqlite read + tag parse run on the backend runtime; the result (which is
/// `Send`) hops back to store the list and seed the counter.
pub(crate) fn build_viewer_slideshow(
    weak: slint::Weak<DarkMatterLinux>,
    backend_cell: Arc<Mutex<Option<Arc<Backend>>>>,
    group_ids: Arc<Mutex<Vec<String>>>,
    group_hex: String,
    current_key: String,
) {
    let Some(backend) = backend_cell.lock().unwrap().clone() else {
        return;
    };
    let handle = backend.tokio_handle();
    handle.spawn(async move {
        let all = backend
            .messages(&group_hex, Some(msg_window_for(&group_hex)))
            .unwrap_or_default();
        let items = build_viewer_items(&all);
        let pos = items
            .iter()
            .position(|it| it.cache_key == current_key)
            .unwrap_or(0);
        let count = items.len();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            // Store the list, then load the selected image (cache hit → instant,
            // miss → loading pill + download).
            let current = VIEWER_SLIDESHOW.with(|s| {
                *s.borrow_mut() = ViewerSlideshow { items, pos };
                s.borrow().items.get(pos).cloned()
            });
            ui.set_image_viewer_count(count as i32);
            ui.set_image_viewer_index((pos + 1) as i32);
            if let Some(item) = current {
                load_viewer_image(&ui, &backend_cell, &group_ids, pos, item);
            } else {
                ui.set_image_viewer_loading(false);
            }
        });
    });
}

/// Show the image at slideshow position `pos` in the lightbox. Cache hit →
/// swap instantly; miss → flip on the loading pill and download+decode, then
/// swap *only if* the viewer is still parked on the same image (the user may
/// have clicked past it). The decoded pixels seed the shared attachment cache
/// so the bubble row and a re-open are both free afterwards.
pub(crate) fn load_viewer_image(
    ui: &DarkMatterLinux,
    backend_cell: &Arc<Mutex<Option<Arc<Backend>>>>,
    group_ids: &Arc<Mutex<Vec<String>>>,
    pos: usize,
    item: ViewerItem,
) {
    if let Some(pixels) = attachment_image_cache_get(&item.cache_key) {
        ui.set_image_viewer_image(image_from_pixels(&pixels));
        ui.set_image_viewer_loading(false);
        return;
    }
    ui.set_image_viewer_loading(true);
    let idx = ui.get_active_chat() as usize;
    let Some(group_hex) = group_ids.lock().unwrap().get(idx).cloned() else {
        return;
    };
    let Some(backend) = backend_cell.lock().unwrap().clone() else {
        return;
    };
    let weak = ui.as_weak();
    let mid = item.cache_key.clone();
    backend.download_media_async(&group_hex, item.reference, move |result| {
        // Runs on the backend runtime. Decode here, hop to the UI thread to
        // build the (!Send) Image and apply it.
        let pixels = match result {
            Ok(dl) => image::load_from_memory(&dl.plaintext).ok().map(|img| {
                let rgba = img.to_rgba8();
                PicturePixels {
                    w: rgba.width(),
                    h: rgba.height(),
                    rgba: rgba.into_raw(),
                }
            }),
            Err(e) => {
                eprintln!("[viewer] download {mid}: {e:#}");
                None
            }
        };
        if let Some(px) = &pixels {
            attachment_image_cache_put(mid.clone(), px.clone());
        }
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            // Drop the result if the user navigated on while we downloaded.
            let still_current = VIEWER_SLIDESHOW.with(|s| {
                let s = s.borrow();
                s.pos == pos && s.items.get(pos).is_some_and(|it| it.cache_key == mid)
            });
            if !still_current {
                return;
            }
            if let Some(px) = pixels {
                ui.set_image_viewer_image(image_from_pixels(&px));
            }
            ui.set_image_viewer_loading(false);
        });
    });
}

/// Resolve a chat record's NIP-92 `imeta` tag into a [`MediaAttachmentReference`]
/// (encrypted-media v1). The tag carries repeatable `locator <kind> <value>`
/// entries plus `ciphertext_sha256` / `plaintext_sha256` / `nonce` / `m` /
/// `filename` / `v` and optional `dim` / `thumbhash`. `source_epoch` is the
/// message's MLS epoch (from the record), needed to derive the per-epoch media
/// secret on download. Returns None when the tag is absent or missing a
/// required field.
pub(crate) fn parse_media_reference_from_tags(
    tags: &[Vec<String>],
    source_epoch: Option<u64>,
) -> Option<MediaAttachmentReference> {
    tags.iter()
        .find(|t| t.first().map(String::as_str) == Some("imeta"))
        .and_then(|t| parse_one_imeta(t, source_epoch))
}

/// Every `imeta` reference on a record, in tag order. A multi-image message
/// (album) carries one `imeta` tag per image; the single-attachment path only
/// ever looks at the first via [`parse_media_reference_from_tags`].
pub(crate) fn parse_all_media_references(
    tags: &[Vec<String>],
    source_epoch: Option<u64>,
) -> Vec<MediaAttachmentReference> {
    tags.iter()
        .filter(|t| t.first().map(String::as_str) == Some("imeta"))
        .filter_map(|t| parse_one_imeta(t, source_epoch))
        .collect()
}

/// Parse a single `imeta` tag (already known to start with "imeta") into a
/// [`MediaAttachmentReference`]. Returns None when a required field is absent.
pub(crate) fn parse_one_imeta(
    tag: &[String],
    source_epoch: Option<u64>,
) -> Option<MediaAttachmentReference> {
    if tag.first().map(String::as_str) != Some("imeta") {
        return None;
    }
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

pub(crate) fn mime_is_image(mime: &str) -> bool {
    mime.starts_with("image/")
}

pub(crate) fn mime_is_video(mime: &str) -> bool {
    mime.starts_with("video/")
}

pub(crate) fn mime_is_audio(mime: &str) -> bool {
    mime.starts_with("audio/")
}

/// Attachment-image-cache key for a video's poster frame. Distinct from the
/// bare message id (which the image path uses) so a video never trips the
/// image lightbox's "already decoded → open viewer" shortcut in
/// `on_attachment_clicked`.
pub(crate) fn vidposter_key(message_id: &str) -> String {
    format!("vidposter:{message_id}")
}

/// Format a duration in seconds as "m:ss" (or "h:mm:ss").
pub(crate) fn fmt_dur(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "0:00".to_string();
    }
    let total = secs.round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Cached duration label per video message id ("1:23"), captured the first
/// time a clip is played (mpv reports `duration`). Renders on the poster tile.
pub(crate) fn video_meta() -> &'static Mutex<HashMap<String, String>> {
    use std::sync::OnceLock;
    static M: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn video_duration_label(message_id: &str) -> String {
    video_meta()
        .lock()
        .ok()
        .and_then(|m| m.get(message_id).cloned())
        .unwrap_or_default()
}

/// The single live [`mpv::MpvPlayer`] backing the video viewer. Only one video
/// plays at a time; opening another or dismissing the viewer drops this (which
/// joins the render/event threads and frees the mpv handle).
pub(crate) fn current_player() -> &'static Mutex<Option<mpv::MpvPlayer>> {
    use std::sync::OnceLock;
    static P: OnceLock<Mutex<Option<mpv::MpvPlayer>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(None))
}

/// Stop + drop the live player off the UI thread. `MpvPlayer::drop` joins its
/// render/event threads and calls `mpv_terminate_destroy`, which can block
/// briefly — never do that on the event loop (hard rule).
pub(crate) fn stop_current_player() {
    let taken = current_player().lock().ok().and_then(|mut p| p.take());
    if let Some(player) = taken {
        std::thread::spawn(move || drop(player));
    }
}

/// Duration (seconds) of the currently-open video, for translating the seek
/// bar's 0..1 fraction into an absolute position.
pub(crate) fn current_video_duration() -> &'static Mutex<f64> {
    use std::sync::OnceLock;
    static D: OnceLock<Mutex<f64>> = OnceLock::new();
    D.get_or_init(|| Mutex::new(0.0))
}

/// `(group_hex, message_id)` of the video currently open in the viewer, so the
/// dismiss handler can repaint that bubble (poster + duration now cached).
pub(crate) fn current_video_target() -> &'static Mutex<Option<(String, String)>> {
    use std::sync::OnceLock;
    static T: OnceLock<Mutex<Option<(String, String)>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(None))
}

/// Whether the video viewer put the app window into fullscreen. Tracked so the
/// `f`-key / button toggle can flip it and the dismiss handler can revert it
/// (so closing the viewer never leaves the whole app stuck fullscreen).
pub(crate) fn video_fullscreen() -> &'static std::sync::atomic::AtomicBool {
    use std::sync::OnceLock;
    static F: OnceLock<std::sync::atomic::AtomicBool> = OnceLock::new();
    F.get_or_init(|| std::sync::atomic::AtomicBool::new(false))
}

/// Fetch (cache read-through, else decrypt+download) a video attachment and
/// hand the bytes to [`spawn_video_player`]. Runs entirely on the backend
/// runtime; on failure it just clears the viewer's loading spinner.
pub(crate) fn start_video_playback(
    weak: Weak<DarkMatterLinux>,
    backend: Arc<Backend>,
    group_hex: String,
    mid: String,
    reference: MediaAttachmentReference,
    vault: Option<Arc<Mutex<Vault>>>,
) {
    let hash = reference.ciphertext_sha256.clone();
    let backend2 = backend.clone();
    backend.tokio_handle().spawn(async move {
        // Encrypted disk cache first (survives restart, no network).
        if let Some(bytes) = vault.as_ref().and_then(|v| media_cache::get(v, &hash)) {
            spawn_video_player(weak, mid, bytes);
            return;
        }
        let weak_fail = weak.clone();
        let mid_dl = mid.clone();
        let vault2 = vault.clone();
        backend2.download_media_async(&group_hex, reference, move |res| match res {
            Ok(dl) => {
                if let Some(v) = &vault2 {
                    media_cache::put(v, &hash, &dl.plaintext);
                }
                spawn_video_player(weak, mid_dl, dl.plaintext);
            }
            Err(e) => {
                eprintln!("[video] download {mid_dl}: {e:#}");
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak_fail.upgrade() {
                        ui.set_video_viewer_loading(false);
                    }
                });
            }
        });
    });
}

/// Build the libmpv player for already-decrypted `bytes`, wiring its frame and
/// state callbacks to the video viewer. Caches the first frame as the bubble
/// poster and the clip duration. Stores the player in [`current_player`] so the
/// viewer controls + dismiss can reach it. Safe to call off the UI thread.
pub(crate) fn spawn_video_player(weak: Weak<DarkMatterLinux>, mid: String, bytes: Vec<u8>) {
    use std::sync::atomic::{AtomicBool, Ordering};

    let poster_saved = Arc::new(AtomicBool::new(false));
    let dur_saved = Arc::new(AtomicBool::new(false));

    let on_frame = {
        let weak = weak.clone();
        let mid = mid.clone();
        move |px: PicturePixels| {
            // First frame doubles as the bubble poster (cached once).
            if !poster_saved.swap(true, Ordering::AcqRel) {
                attachment_image_cache_put(vidposter_key(&mid), px.clone());
            }
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_video_viewer_frame(image_from_pixels(&px));
                    ui.set_video_viewer_has_frame(true);
                    ui.set_video_viewer_loading(false);
                }
            });
        }
    };

    let on_state = {
        let weak = weak.clone();
        let mid = mid.clone();
        move |st: mpv::PlayerState| {
            if st.duration > 0.0 {
                *current_video_duration().lock().unwrap() = st.duration;
                if !dur_saved.swap(true, Ordering::AcqRel)
                    && let Ok(mut m) = video_meta().lock()
                {
                    m.insert(mid.clone(), fmt_dur(st.duration));
                }
            }
            let progress = if st.duration > 0.0 {
                (st.time_pos / st.duration).clamp(0.0, 1.0) as f32
            } else {
                0.0
            };
            let pos_l = fmt_dur(st.time_pos);
            let dur_l = fmt_dur(st.duration);
            let playing = !st.paused;
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_video_viewer_progress(progress);
                    ui.set_video_viewer_pos(pos_l.into());
                    ui.set_video_viewer_dur(dur_l.into());
                    ui.set_video_viewer_playing(playing);
                }
            });
        }
    };

    match mpv::MpvPlayer::open(bytes, 1920, on_frame, on_state) {
        Some(player) => {
            *current_player().lock().unwrap() = Some(player);
        }
        None => {
            eprintln!("[video] mpv player failed to start");
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_video_viewer_loading(false);
                }
            });
        }
    }
}

// ─── Voice-message playback helpers ─────────────────────────────────────────

/// Stop any active voice-message playback. Must be called from the Slint UI
/// thread because the player is !Send.
pub(crate) fn stop_current_audio() {
    with_active_player(|p| {
        *p = None;
    });
    *current_audio_message_id().lock().unwrap() = None;
}

/// Start playing an audio attachment. `bytes` are the decrypted WAV data.
/// A monitor thread keeps the playing message's bubble refreshed with
/// position/duration. When playback finishes or another message is started,
/// the bubble is updated accordingly.
pub(crate) fn start_audio_playback(
    weak: Weak<DarkMatterLinux>,
    backend_cell: Arc<Mutex<Option<Arc<Backend>>>>,
    group_ids: Arc<Mutex<Vec<String>>>,
    pending_state: Arc<Mutex<PendingState>>,
    group_hex: String,
    message_id: String,
    bytes: Vec<u8>,
) {
    stop_current_audio();
    let player = match audio::AudioPlayer::play(bytes) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[audio] play {message_id}: {e:#}");
            return;
        }
    };

    let mid = message_id.clone();
    let weak2 = weak.clone();
    let backend_cell_c = backend_cell.clone();
    let group_ids_c = group_ids.clone();
    let pending_state_c = pending_state.clone();
    let group_hex_c = group_hex.clone();
    player.spawn_monitor(move |st: audio::PlaybackState| {
        let progress = if st.duration > 0.0 {
            (st.position / st.duration).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };
        {
            let mut cache = audio_progress().lock().unwrap();
            if st.finished {
                cache.insert(mid.clone(), 0.0);
            } else {
                cache.insert(mid.clone(), progress);
            }
        }
        if st.duration > 0.0
            && let Ok(mut m) = audio_meta().lock()
        {
            m.insert(mid.clone(), fmt_dur(st.duration));
        }
        let finished = st.finished;
        let mid_i = mid.clone();
        let mid_fin = mid.clone();
        let weak_i = weak2.clone();
        let backend_cell_i = backend_cell_c.clone();
        let group_ids_i = group_ids_c.clone();
        let pending_state_i = pending_state_c.clone();
        let group_hex_i = group_hex_c.clone();
        let _ = slint::invoke_from_event_loop(move || {
            // A drained clip is no longer the active player. Drop it (only if it
            // is still the current one — a newer clip may have started) so the
            // next play-button press restarts from the top instead of toggling
            // an empty sink, which does nothing.
            let still_current = finished
                && current_audio_message_id().lock().unwrap().as_deref() == Some(mid_fin.as_str());
            if still_current {
                stop_current_audio();
            }
            if let Some(ui) = weak_i.upgrade() {
                // Refresh the bubble so the play button / progress bar updates.
                let idx = ui.get_active_chat() as usize;
                let group_opt = {
                    let ids = group_ids_i.lock().unwrap();
                    ids.get(idx).cloned()
                };
                if let Some(g) = group_opt
                    && g == group_hex_i
                    && let Some(backend) = backend_cell_i.lock().unwrap().clone()
                {
                    refresh_one_message_row_async(
                        &backend,
                        weak_i,
                        pending_state_i,
                        group_ids_i,
                        group_hex_i,
                        mid_i,
                    );
                }
            }
        });
    });

    with_active_player(|p| {
        *p = Some(player);
    });
    *current_audio_message_id().lock().unwrap() = Some(message_id);
}

// ─── Album (multi-image) layout + cells ────────────────────────────────────

/// Per-image cache/slideshow key. A lone attachment keeps the bare message id
/// (back-compat with the single-image path); album images are `id#index`.
pub(crate) fn att_key(message_id: &str, index: usize) -> String {
    format!("{message_id}#{index}")
}

/// Aspect ratio (w/h) from an `imeta` `dim "WxH"` field, if present + valid.
pub(crate) fn parse_dim_ar(dim: &Option<String>) -> Option<f32> {
    let d = dim.as_ref()?;
    let (w, h) = d.split_once(['x', 'X'])?;
    let w: f32 = w.trim().parse().ok()?;
    let h: f32 = h.trim().parse().ok()?;
    (w > 0.0 && h > 0.0).then_some(w / h)
}

pub(crate) const ALBUM_GAP: f32 = 3.0;
pub(crate) const ALBUM_MAX_H: f32 = 460.0;

/// Telegram-style aspect-aware grid. Given each image's aspect ratio, lay the
/// album into a box `max_w` wide and return per-cell px rects `(x, y, w, h)`
/// plus the total height. Special cases for 2/3 images (the eye-catching
/// arrangements); a balanced justified-rows fallback for 4+. The whole grid is
/// scaled down if it would exceed `ALBUM_MAX_H`.
pub(crate) fn album_layout(
    aspects: &[f32],
    max_w: f32,
    sp: f32,
) -> (Vec<(f32, f32, f32, f32)>, f32) {
    let n = aspects.len();
    // Clamp extreme panoramas/strips so one wild image can't wreck the grid.
    let ar: Vec<f32> = aspects.iter().map(|a| a.clamp(0.5, 2.4)).collect();
    let (mut out, total_h): (Vec<(f32, f32, f32, f32)>, f32) = match n {
        0 => (vec![], 0.0),
        1 => {
            let h = (max_w / ar[0]).clamp(max_w * 0.5, max_w * 1.4);
            (vec![(0.0, 0.0, max_w, h)], h)
        }
        2 if ar[0] > 1.2 && ar[1] > 1.2 => {
            // Two wide images read best stacked full-width.
            let h0 = max_w / ar[0];
            let h1 = max_w / ar[1];
            (
                vec![(0.0, 0.0, max_w, h0), (0.0, h0 + sp, max_w, h1)],
                h0 + sp + h1,
            )
        }
        2 => {
            // Two equal-height columns filling the width.
            let h = (max_w - sp) / (ar[0] + ar[1]);
            let w0 = h * ar[0];
            (
                vec![(0.0, 0.0, w0, h), (w0 + sp, 0.0, (max_w - sp) - w0, h)],
                h,
            )
        }
        3 if ar[0] >= 1.0 => {
            // One wide image on top, two columns below.
            let h0 = (max_w / ar[0]).clamp(max_w * 0.4, max_w * 0.9);
            let hb = (max_w - sp) / (ar[1] + ar[2]);
            let w1 = hb * ar[1];
            (
                vec![
                    (0.0, 0.0, max_w, h0),
                    (0.0, h0 + sp, w1, hb),
                    (w1 + sp, h0 + sp, (max_w - sp) - w1, hb),
                ],
                h0 + sp + hb,
            )
        }
        3 => {
            // One tall image on the left, two stacked on the right.
            let inv = 1.0 / ar[1] + 1.0 / ar[2];
            let wr = ((max_w - sp - ar[0] * sp) / (ar[0] * inv + 1.0)).clamp(50.0, max_w - 70.0);
            let wl = max_w - sp - wr;
            let big_h = wl / ar[0];
            let h1 = (wr / ar[1]).min(big_h - sp - 20.0).max(20.0);
            let h2 = (big_h - sp - h1).max(20.0);
            (
                vec![
                    (0.0, 0.0, wl, big_h),
                    (wl + sp, 0.0, wr, h1),
                    (wl + sp, h1 + sp, wr, h2),
                ],
                big_h,
            )
        }
        _ => {
            // Balanced justified rows: split into ~sqrt(n) rows, earlier rows
            // take the remainder, each row justified to fill `max_w`.
            let rows = ((n as f32).sqrt().round() as usize).clamp(2, n);
            let base = n / rows;
            let extra = n % rows;
            let mut sizes = vec![base; rows];
            for s in sizes.iter_mut().take(extra) {
                *s += 1;
            }
            let mut rects = Vec::with_capacity(n);
            let (mut y, mut idx) = (0.0_f32, 0usize);
            for &k in &sizes {
                let row_ar: f32 = (0..k).map(|j| ar[idx + j]).sum();
                let avail = max_w - sp * ((k as f32) - 1.0);
                let h = avail / row_ar;
                let mut x = 0.0_f32;
                for j in 0..k {
                    // Last cell absorbs rounding so the row fills exactly.
                    let w = if j == k - 1 {
                        max_w - x
                    } else {
                        h * ar[idx + j]
                    };
                    rects.push((x, y, w, h));
                    x += w + sp;
                }
                y += h + sp;
                idx += k;
            }
            (rects, (y - sp).max(0.0))
        }
    };
    if total_h > ALBUM_MAX_H {
        let scale = ALBUM_MAX_H / total_h;
        for r in out.iter_mut() {
            r.0 *= scale;
            r.1 *= scale;
            r.2 *= scale;
            r.3 *= scale;
        }
        return (out, ALBUM_MAX_H);
    }
    (out, total_h)
}

/// Album width for a bubble (outgoing bubbles are narrower than incoming).
pub(crate) fn album_box_w(outgoing: bool) -> f32 {
    if outgoing { 360.0 } else { 380.0 }
}

/// Build the grid cells for a confirmed album message: geometry from each
/// image's `dim` (or cached pixels, or square), images from the attachment
/// cache (placeholder until a cell decodes).
pub(crate) fn build_album_cells(
    references: &[MediaAttachmentReference],
    message_id: &str,
    outgoing: bool,
) -> (Vec<AlbumCell>, f32, f32) {
    let max_w = album_box_w(outgoing);
    let aspects: Vec<f32> = references
        .iter()
        .enumerate()
        .map(|(i, r)| {
            parse_dim_ar(&r.dim)
                .or_else(|| {
                    attachment_image_cache_get(&att_key(message_id, i))
                        .map(|p| p.w as f32 / (p.h.max(1) as f32))
                })
                .unwrap_or(1.0)
        })
        .collect();
    let (rects, total_h) = album_layout(&aspects, max_w, ALBUM_GAP);
    let cells = references
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let key = att_key(message_id, i);
            let (image, has_image) = match attachment_image_cache_get(&key) {
                Some(p) => (image_from_pixels(&p), true),
                None => (slint::Image::default(), false),
            };
            let loading = attachment_in_flight()
                .lock()
                .map(|s| s.contains(&key))
                .unwrap_or(false);
            let (x, y, w, h) = rects[i];
            AlbumCell {
                x,
                y,
                w,
                h,
                image,
                has_image,
                loading,
                key: key.into(),
            }
        })
        .collect();
    (cells, max_w, total_h)
}

/// Build the grid cells for a pending (optimistic) album from the local
/// previews the user picked — always rendered, no download needed.
pub(crate) fn pending_album_cells(
    media: &[PendingMedia],
    temp_id: &str,
    outgoing: bool,
) -> (Vec<AlbumCell>, f32, f32) {
    let max_w = album_box_w(outgoing);
    let aspects: Vec<f32> = media
        .iter()
        .map(|m| {
            m.local_preview
                .as_ref()
                .map(|p| p.w as f32 / (p.h.max(1) as f32))
                .unwrap_or(1.0)
        })
        .collect();
    let (rects, total_h) = album_layout(&aspects, max_w, ALBUM_GAP);
    let cells = media
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let (image, has_image) = match &m.local_preview {
                Some(p) => (image_from_pixels(p), true),
                None => (slint::Image::default(), false),
            };
            let (x, y, w, h) = rects[i];
            AlbumCell {
                x,
                y,
                w,
                h,
                image,
                has_image,
                loading: false,
                key: att_key(temp_id, i).into(),
            }
        })
        .collect();
    (cells, max_w, total_h)
}

/// Empty album fields for the common non-album row.
pub(crate) fn no_album() -> (ModelRc<AlbumCell>, f32, f32) {
    (
        ModelRc::new(VecModel::from(Vec::<AlbumCell>::new())),
        0.0,
        0.0,
    )
}

/// Handles album auto-load needs, stashed once at startup. UI-thread-only
/// (a thread-local), so the otherwise-pure row builders can trigger
/// background image fetches without threading these through every signature.
#[derive(Clone)]
pub(crate) struct AlbumLoadCtx {
    pub(crate) weak: Weak<DarkMatterLinux>,
    pub(crate) backend_cell: Arc<Mutex<Option<Arc<Backend>>>>,
    pub(crate) vault_cell: Arc<Mutex<Option<Arc<Mutex<Vault>>>>>,
    pub(crate) group_ids: Arc<Mutex<Vec<String>>>,
    pub(crate) pending_state: Arc<Mutex<PendingState>>,
}

thread_local! {
    static ALBUM_LOAD_CTX: std::cell::RefCell<Option<AlbumLoadCtx>> =
        const { std::cell::RefCell::new(None) };
}

pub(crate) fn set_album_load_ctx(ctx: AlbumLoadCtx) {
    ALBUM_LOAD_CTX.with(|c| *c.borrow_mut() = Some(ctx));
}

/// For an album record (2+ images), kick off background download+decode for
/// any cell that isn't already cached — so incoming albums, and our own
/// albums after a restart cleared the in-memory cache, fill their grid in
/// instead of showing placeholders. No-op for cached/in-flight cells. Each
/// finished cell seeds the in-memory + disk caches and refreshes its row.
/// Reads through the encrypted disk cache before paying for a download.
pub(crate) fn maybe_autoload_album(group_hex: &str, record: &AppMessageRecord) {
    let images: Vec<MediaAttachmentReference> =
        parse_all_media_references(&record.tags, record.source_epoch)
            .into_iter()
            .filter(|r| mime_is_image(&r.media_type))
            .collect();
    if images.len() < 2 {
        return;
    }
    // Which cells still need fetching? Cheap checks only — the attachment
    // cache + in-flight set are independent mutexes. Crucially we do NOT lock
    // `backend_cell` here: this runs from the row builders, which are usually
    // called while the caller already holds that guard, and `std::sync::Mutex`
    // is non-reentrant — re-locking it on the same thread would deadlock.
    let needed: Vec<(usize, MediaAttachmentReference)> = images
        .into_iter()
        .enumerate()
        .filter(|(i, _)| {
            let key = att_key(&record.message_id_hex, *i);
            attachment_image_cache_get(&key).is_none()
                && !attachment_in_flight()
                    .lock()
                    .map(|s| s.contains(&key))
                    .unwrap_or(true)
        })
        .collect();
    if needed.is_empty() {
        return;
    }
    // Defer the backend acquisition + downloads to a fresh event-loop turn so
    // any `backend_cell` guard held by the current row build is released first.
    let group_hex = group_hex.to_string();
    let mid = record.message_id_hex.clone();
    let _ = slint::invoke_from_event_loop(move || autoload_album_cells(group_hex, mid, needed));
}

/// Backend half of [`maybe_autoload_album`], run on its own event-loop turn so
/// no row-builder `backend_cell` guard is in scope. Disk read-through →
/// download → seed caches → refresh the row, per still-missing cell.
pub(crate) fn autoload_album_cells(
    group_hex: String,
    mid: String,
    needed: Vec<(usize, MediaAttachmentReference)>,
) {
    let Some(ctx) = ALBUM_LOAD_CTX.with(|c| c.borrow().clone()) else {
        return;
    };
    let Some(backend) = ctx.backend_cell.lock().unwrap().clone() else {
        return;
    };
    let vault = ctx.vault_cell.lock().unwrap().clone();
    for (i, reference) in needed {
        let key = att_key(&mid, i);
        if attachment_image_cache_get(&key).is_some() {
            continue;
        }
        {
            let Ok(mut set) = attachment_in_flight().lock() else {
                continue;
            };
            if set.contains(&key) {
                continue;
            }
            set.insert(key.clone());
        }
        let backend = backend.clone();
        let vault = vault.clone();
        let group_hex = group_hex.clone();
        let mid = mid.clone();
        let weak = ctx.weak.clone();
        let group_ids = ctx.group_ids.clone();
        let pending_state = ctx.pending_state.clone();
        let hash = reference.ciphertext_sha256.clone();
        backend.tokio_handle().spawn({
            let backend = backend.clone();
            async move {
                // Disk read-through: skip the network if we cached the bytes.
                if let Some(px) = vault
                    .as_ref()
                    .and_then(|v| media_cache::get(v, &hash))
                    .and_then(|plain| decode_avatar_pixels(&plain).ok())
                {
                    attachment_image_cache_put(key.clone(), px);
                    attachment_in_flight()
                        .lock()
                        .ok()
                        .map(|mut s| s.remove(&key));
                    refresh_one_message_row_async(
                        &backend,
                        weak,
                        pending_state,
                        group_ids,
                        group_hex,
                        mid,
                    );
                    return;
                }
                let backend_cb = backend.clone();
                let group_hex_cb = group_hex.clone();
                backend.download_media_async(&group_hex, reference, move |result| {
                    let pixels = match result {
                        Ok(dl) => {
                            if let Some(v) = &vault {
                                media_cache::put(v, &hash, &dl.plaintext);
                            }
                            decode_avatar_pixels(&dl.plaintext).ok()
                        }
                        Err(e) => {
                            eprintln!("[album] autoload {key}: {e:#}");
                            None
                        }
                    };
                    let ok = pixels.is_some();
                    if let Some(px) = pixels {
                        attachment_image_cache_put(key.clone(), px);
                    }
                    attachment_in_flight()
                        .lock()
                        .ok()
                        .map(|mut s| s.remove(&key));
                    if ok {
                        refresh_one_message_row_async(
                            &backend_cb,
                            weak,
                            pending_state,
                            group_ids,
                            group_hex_cb,
                            mid,
                        );
                    }
                });
            }
        });
    }
}

/// Build a [`StagedFile`] from raw bytes: full-resolution decode for the
/// optimistic bubble preview plus a ≤96px thumbnail for the composer chip.
/// Blocking image decode — call off the UI thread.
pub(crate) fn staged_file_from_bytes(
    file_name: String,
    media_type: String,
    bytes: Vec<u8>,
) -> StagedFile {
    let is_image = mime_is_image(&media_type);
    let (preview, thumb) = if is_image {
        match image::load_from_memory(&bytes) {
            Ok(img) => {
                let t = img.thumbnail(96, 96).to_rgba8();
                let thumb = PicturePixels {
                    w: t.width(),
                    h: t.height(),
                    rgba: t.into_raw(),
                };
                let rgba = img.to_rgba8();
                let (w, h) = (rgba.width(), rgba.height());
                (
                    Some(PicturePixels {
                        w,
                        h,
                        rgba: rgba.into_raw(),
                    }),
                    Some(thumb),
                )
            }
            Err(_) => (None, None),
        }
    } else {
        (None, None)
    };
    StagedFile {
        file_name,
        media_type,
        bytes,
        is_image,
        preview,
        thumb,
    }
}

/// Rebuild the composer's staged-attachment chip row from the queue. UI
/// thread only — it constructs `slint::Image` thumbnails.
pub(crate) fn refresh_staged_ui(ui: &DarkMatterLinux, staged: &[StagedFile]) {
    let rows: Vec<StagedAttachment> = staged
        .iter()
        .map(|f| StagedAttachment {
            name: f.file_name.clone().into(),
            size_label: human_bytes(f.bytes.len() as u64).into(),
            is_image: f.is_image,
            thumb: f.thumb.as_ref().map(image_from_pixels).unwrap_or_default(),
            has_thumb: f.thumb.is_some(),
        })
        .collect();
    ui.set_composer_staged(ModelRc::new(VecModel::from(rows)));
}

/// Compact byte-size label for attachment chips. KB/MB rounded to one decimal.
/// Map on-disk audit-log files into UI rows (newest first) and push the model.
pub(crate) fn push_audit_files(ui: &DarkMatterLinux, mut files: Vec<AuditLogFile>) {
    files.sort_by(|a, b| {
        b.modified_at_ms
            .unwrap_or(0)
            .cmp(&a.modified_at_ms.unwrap_or(0))
    });
    let rows: Vec<AuditLogEntry> = files
        .iter()
        .map(|f| AuditLogEntry {
            path: f.path.clone().into(),
            name: f.file_name.clone().into(),
            meta: match f.modified_at_ms {
                Some(ms) => format!(
                    "{} · {}",
                    human_bytes(f.size_bytes),
                    format_date_unix(ms / 1000)
                )
                .into(),
                None => human_bytes(f.size_bytes).into(),
            },
        })
        .collect();
    ui.set_audit_files(ModelRc::new(VecModel::from(rows)));
}

/// List audit-log files off the UI thread (disk IO) and push the rows back
/// through the event loop.
pub(crate) fn refresh_audit_files(ui: &DarkMatterLinux, backend: &Arc<Backend>) {
    let weak = ui.as_weak();
    let b = backend.clone();
    backend.tokio_handle().spawn(async move {
        let files = b.audit_log_files().unwrap_or_else(|e| {
            eprintln!("[settings] list audit logs failed: {e:#}");
            Vec::new()
        });
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                push_audit_files(&ui, files);
            }
        });
    });
}

pub(crate) fn human_bytes(n: u64) -> String {
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
