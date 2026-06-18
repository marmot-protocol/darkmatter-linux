//! In-app video playback via embedded **libmpv** (`libmpv.so.2`).
//!
//! Chat attachments are encrypted MIP-04 blobs decrypted in-process — there is
//! no URL to hand an external player, and (per this project's ethos) we never
//! write decrypted bytes to disk in plaintext. So we feed mpv the decrypted
//! bytes straight from memory through its **read-only stream callback** API
//! (a custom `dmblob://` protocol) and render frames with mpv's **software
//! render API** (`MPV_RENDER_API_TYPE_SW`), which blits into a CPU buffer that
//! drops straight into the app's `PicturePixels → slint::Image` pipeline. mpv
//! owns audio output and A/V sync, so we get synced sound for free.
//!
//! Threading: mpv's C API is thread-safe. An [`MpvPlayer`] owns the handle +
//! render context and spawns two helper threads — a *render* thread (woken by
//! mpv's render-update callback, blits the current frame and ships it to the
//! UI) and an *event* thread (pumps `mpv_wait_event` and reports time/duration/
//! pause/eof). Both are torn down on `Drop`, before the context + handle are
//! freed, so nothing dangles.
//!
//! All FFI is hand-rolled against the system headers (`client.h`, `render.h`,
//! `stream_cb.h`) — we only need a handful of entry points, and a crate would
//! add a guess about software-render coverage. `build.rs` links `-lmpv`.

#![allow(non_camel_case_types)]

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use crate::PicturePixels;

// ─── Raw FFI ────────────────────────────────────────────────────────────────

#[repr(C)]
struct mpv_handle {
    _private: [u8; 0],
}
#[repr(C)]
struct mpv_render_context {
    _private: [u8; 0],
}

// mpv_format
const MPV_FORMAT_FLAG: c_int = 3;
const MPV_FORMAT_INT64: c_int = 4;
const MPV_FORMAT_DOUBLE: c_int = 5;

// mpv_event_id
const MPV_EVENT_NONE: c_int = 0;
const MPV_EVENT_SHUTDOWN: c_int = 1;
const MPV_EVENT_END_FILE: c_int = 7;
const MPV_EVENT_PROPERTY_CHANGE: c_int = 22;

// mpv_render_param_type
const MPV_RENDER_PARAM_INVALID: c_int = 0;
const MPV_RENDER_PARAM_API_TYPE: c_int = 1;
const MPV_RENDER_PARAM_SW_SIZE: c_int = 17;
const MPV_RENDER_PARAM_SW_FORMAT: c_int = 18;
const MPV_RENDER_PARAM_SW_STRIDE: c_int = 19;
const MPV_RENDER_PARAM_SW_POINTER: c_int = 20;

const MPV_RENDER_UPDATE_FRAME: u64 = 1;

#[repr(C)]
struct mpv_event {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

#[repr(C)]
struct mpv_event_property {
    name: *const c_char,
    format: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct mpv_render_param {
    type_: c_int,
    data: *mut c_void,
}

type mpv_stream_cb_read_fn =
    Option<unsafe extern "C" fn(cookie: *mut c_void, buf: *mut c_char, nbytes: u64) -> i64>;
type mpv_stream_cb_seek_fn = Option<unsafe extern "C" fn(cookie: *mut c_void, offset: i64) -> i64>;
type mpv_stream_cb_size_fn = Option<unsafe extern "C" fn(cookie: *mut c_void) -> i64>;
type mpv_stream_cb_close_fn = Option<unsafe extern "C" fn(cookie: *mut c_void)>;
type mpv_stream_cb_cancel_fn = Option<unsafe extern "C" fn(cookie: *mut c_void)>;

#[repr(C)]
struct mpv_stream_cb_info {
    cookie: *mut c_void,
    read_fn: mpv_stream_cb_read_fn,
    seek_fn: mpv_stream_cb_seek_fn,
    size_fn: mpv_stream_cb_size_fn,
    close_fn: mpv_stream_cb_close_fn,
    cancel_fn: mpv_stream_cb_cancel_fn,
}

type mpv_stream_cb_open_ro_fn = Option<
    unsafe extern "C" fn(
        user_data: *mut c_void,
        uri: *mut c_char,
        info: *mut mpv_stream_cb_info,
    ) -> c_int,
>;

type mpv_render_update_fn = Option<unsafe extern "C" fn(cb_ctx: *mut c_void)>;

#[link(name = "mpv")]
unsafe extern "C" {
    fn mpv_create() -> *mut mpv_handle;
    fn mpv_initialize(ctx: *mut mpv_handle) -> c_int;
    fn mpv_terminate_destroy(ctx: *mut mpv_handle);
    fn mpv_set_option_string(
        ctx: *mut mpv_handle,
        name: *const c_char,
        data: *const c_char,
    ) -> c_int;
    fn mpv_set_property(
        ctx: *mut mpv_handle,
        name: *const c_char,
        format: c_int,
        data: *mut c_void,
    ) -> c_int;
    fn mpv_get_property(
        ctx: *mut mpv_handle,
        name: *const c_char,
        format: c_int,
        data: *mut c_void,
    ) -> c_int;
    fn mpv_command(ctx: *mut mpv_handle, args: *mut *const c_char) -> c_int;
    fn mpv_observe_property(
        ctx: *mut mpv_handle,
        reply_userdata: u64,
        name: *const c_char,
        format: c_int,
    ) -> c_int;
    fn mpv_wait_event(ctx: *mut mpv_handle, timeout: f64) -> *mut mpv_event;
    fn mpv_wakeup(ctx: *mut mpv_handle);
    fn mpv_stream_cb_add_ro(
        ctx: *mut mpv_handle,
        protocol: *const c_char,
        user_data: *mut c_void,
        open_fn: mpv_stream_cb_open_ro_fn,
    ) -> c_int;

    fn mpv_render_context_create(
        res: *mut *mut mpv_render_context,
        mpv: *mut mpv_handle,
        params: *mut mpv_render_param,
    ) -> c_int;
    fn mpv_render_context_render(
        ctx: *mut mpv_render_context,
        params: *mut mpv_render_param,
    ) -> c_int;
    fn mpv_render_context_update(ctx: *mut mpv_render_context) -> u64;
    fn mpv_render_context_set_update_callback(
        ctx: *mut mpv_render_context,
        callback: mpv_render_update_fn,
        callback_ctx: *mut c_void,
    );
    fn mpv_render_context_free(ctx: *mut mpv_render_context);
}

// ─── Stream callback (in-memory decrypted blob) ──────────────────────────────

// Per-open cursor over the shared bytes. mpv opens the stream once per
// loadfile; `close_fn` frees this box.
struct StreamCookie {
    data: Arc<Vec<u8>>,
    pos: u64,
}

unsafe extern "C" fn stream_read(cookie: *mut c_void, buf: *mut c_char, nbytes: u64) -> i64 {
    let c = unsafe { &mut *(cookie as *mut StreamCookie) };
    let len = c.data.len() as u64;
    if c.pos >= len {
        return 0;
    }
    let n = nbytes.min(len - c.pos) as usize;
    let start = c.pos as usize;
    unsafe {
        std::ptr::copy_nonoverlapping(c.data.as_ptr().add(start), buf as *mut u8, n);
    }
    c.pos += n as u64;
    n as i64
}

unsafe extern "C" fn stream_seek(cookie: *mut c_void, offset: i64) -> i64 {
    let c = unsafe { &mut *(cookie as *mut StreamCookie) };
    if offset < 0 || offset as u64 > c.data.len() as u64 {
        return -1; // MPV_ERROR_GENERIC-ish; mpv treats <0 as failure
    }
    c.pos = offset as u64;
    c.pos as i64
}

unsafe extern "C" fn stream_size(cookie: *mut c_void) -> i64 {
    let c = unsafe { &*(cookie as *const StreamCookie) };
    c.data.len() as i64
}

unsafe extern "C" fn stream_close(cookie: *mut c_void) {
    drop(unsafe { Box::from_raw(cookie as *mut StreamCookie) });
}

// `user_data` is a `*const Arc<Vec<u8>>` we registered (owned by the player /
// leaked for the one-shot grab). Each open clones the Arc into a fresh cursor.
unsafe extern "C" fn stream_open(
    user_data: *mut c_void,
    _uri: *mut c_char,
    info: *mut mpv_stream_cb_info,
) -> c_int {
    let data = unsafe { &*(user_data as *const Arc<Vec<u8>>) };
    let cookie = Box::new(StreamCookie {
        data: data.clone(),
        pos: 0,
    });
    let info = unsafe { &mut *info };
    info.cookie = Box::into_raw(cookie) as *mut c_void;
    info.read_fn = Some(stream_read);
    info.seek_fn = Some(stream_seek);
    info.size_fn = Some(stream_size);
    info.close_fn = Some(stream_close);
    info.cancel_fn = None;
    0
}

// ─── Send-safe pointer wrappers ──────────────────────────────────────────────

#[derive(Clone, Copy)]
struct HandlePtr(*mut mpv_handle);
unsafe impl Send for HandlePtr {}
unsafe impl Sync for HandlePtr {}

#[derive(Clone, Copy)]
struct RenderPtr(*mut mpv_render_context);
unsafe impl Send for RenderPtr {}
unsafe impl Sync for RenderPtr {}

// ─── Shared wake state (render thread ⇄ mpv update callback) ──────────────────

struct Wake {
    redraw: Mutex<bool>,
    cv: Condvar,
    shutdown: AtomicBool,
}

unsafe extern "C" fn render_update_cb(cb_ctx: *mut c_void) {
    // Called from mpv's render thread; keep it trivial — flag + notify.
    let wake = unsafe { &*(cb_ctx as *const Wake) };
    *wake.redraw.lock().unwrap() = true;
    wake.cv.notify_all();
}

// ─── Public state snapshot ───────────────────────────────────────────────────

/// Playback state pushed to the UI on every relevant property change.
#[derive(Clone, Copy, Debug, Default)]
pub struct PlayerState {
    pub time_pos: f64,
    pub duration: f64,
    pub paused: bool,
    pub eof: bool,
}

// ─── Small FFI helpers ───────────────────────────────────────────────────────

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap_or_else(|_| CString::new("").unwrap())
}

unsafe fn get_f64(h: *mut mpv_handle, name: &CStr) -> Option<f64> {
    let mut v: f64 = 0.0;
    let r = unsafe {
        mpv_get_property(
            h,
            name.as_ptr(),
            MPV_FORMAT_DOUBLE,
            &mut v as *mut f64 as *mut c_void,
        )
    };
    (r >= 0).then_some(v)
}

unsafe fn get_i64(h: *mut mpv_handle, name: &CStr) -> Option<i64> {
    let mut v: i64 = 0;
    let r = unsafe {
        mpv_get_property(
            h,
            name.as_ptr(),
            MPV_FORMAT_INT64,
            &mut v as *mut i64 as *mut c_void,
        )
    };
    (r >= 0).then_some(v)
}

unsafe fn get_flag(h: *mut mpv_handle, name: &CStr) -> Option<bool> {
    let mut v: c_int = 0;
    let r = unsafe {
        mpv_get_property(
            h,
            name.as_ptr(),
            MPV_FORMAT_FLAG,
            &mut v as *mut c_int as *mut c_void,
        )
    };
    (r >= 0).then_some(v != 0)
}

// Render the current frame into RGBA pixels at a size derived from the video's
// display dimensions, capped to `max_dim` on the longest side. Returns None
// until mpv has a frame (dwidth/dheight resolve > 0). `fmt`/`buf` are reused
// across calls by the caller. mpv SW format "rgb0" writes [R,G,B,pad]; we set
// the pad byte to 0xFF so the buffer is opaque RGBA for Slint.
unsafe fn render_frame(
    rctx: *mut mpv_render_context,
    h: *mut mpv_handle,
    max_dim: u32,
) -> Option<PicturePixels> {
    let dw = unsafe { get_i64(h, c"dwidth") }.unwrap_or(0);
    let dh = unsafe { get_i64(h, c"dheight") }.unwrap_or(0);
    if dw <= 0 || dh <= 0 {
        return None;
    }
    let (mut w, mut h_px) = (dw as u32, dh as u32);
    let longest = w.max(h_px);
    if longest > max_dim {
        let scale = max_dim as f64 / longest as f64;
        w = ((w as f64 * scale).round() as u32).max(2);
        h_px = ((h_px as f64 * scale).round() as u32).max(2);
    }
    // Even dimensions keep some scalers happy.
    w &= !1;
    h_px &= !1;
    if w == 0 || h_px == 0 {
        return None;
    }

    let stride: usize = w as usize * 4;
    let mut buf = vec![0u8; stride * h_px as usize];
    let mut size = [w as c_int, h_px as c_int];
    let fmt = c"rgb0";
    let mut stride_sz: usize = stride;

    let mut params = [
        mpv_render_param {
            type_: MPV_RENDER_PARAM_SW_SIZE,
            data: size.as_mut_ptr() as *mut c_void,
        },
        mpv_render_param {
            type_: MPV_RENDER_PARAM_SW_FORMAT,
            data: fmt.as_ptr() as *mut c_void,
        },
        mpv_render_param {
            type_: MPV_RENDER_PARAM_SW_STRIDE,
            data: &mut stride_sz as *mut usize as *mut c_void,
        },
        mpv_render_param {
            type_: MPV_RENDER_PARAM_SW_POINTER,
            data: buf.as_mut_ptr() as *mut c_void,
        },
        mpv_render_param {
            type_: MPV_RENDER_PARAM_INVALID,
            data: std::ptr::null_mut(),
        },
    ];
    let r = unsafe { mpv_render_context_render(rctx, params.as_mut_ptr()) };
    if r < 0 {
        return None;
    }
    // rgb0 → opaque RGBA: stamp the pad byte.
    for px in buf.chunks_exact_mut(4) {
        px[3] = 0xFF;
    }
    Some(PicturePixels {
        w,
        h: h_px,
        rgba: buf,
    })
}

// Common handle setup: create + configure + initialize + register the blob
// protocol + create the SW render context. `audio` false uses a null AO (for
// the silent poster grab). Returns (handle, render_ctx, leaked user_data ptr).
// On any failure the partially-built handle is destroyed and None returned.
unsafe fn build_handle(
    bytes: Arc<Vec<u8>>,
    audio: bool,
) -> Option<(*mut mpv_handle, *mut mpv_render_context, *mut Arc<Vec<u8>>)> {
    let h = unsafe { mpv_create() };
    if h.is_null() {
        return None;
    }
    unsafe {
        // vo=libmpv is required for the render API path.
        mpv_set_option_string(h, c"vo".as_ptr(), c"libmpv".as_ptr());
        mpv_set_option_string(h, c"hwdec".as_ptr(), c"no".as_ptr());
        mpv_set_option_string(h, c"audio-display".as_ptr(), c"no".as_ptr());
        if !audio {
            mpv_set_option_string(h, c"ao".as_ptr(), c"null".as_ptr());
            mpv_set_option_string(h, c"pause".as_ptr(), c"yes".as_ptr());
        }
        // Loop the chat clip; users dismiss to stop.
        if audio {
            mpv_set_option_string(h, c"loop-file".as_ptr(), c"inf".as_ptr());
        }
        if mpv_initialize(h) < 0 {
            mpv_terminate_destroy(h);
            return None;
        }
    }

    // Register the in-memory protocol. user_data outlives the handle; freed by
    // the caller after teardown.
    let user_data = Box::into_raw(Box::new(bytes));
    unsafe {
        if mpv_stream_cb_add_ro(
            h,
            c"dmblob".as_ptr(),
            user_data as *mut c_void,
            Some(stream_open),
        ) < 0
        {
            drop(Box::from_raw(user_data));
            mpv_terminate_destroy(h);
            return None;
        }
    }

    // SW render context.
    let mut rctx: *mut mpv_render_context = std::ptr::null_mut();
    let api = c"sw";
    let mut params = [
        mpv_render_param {
            type_: MPV_RENDER_PARAM_API_TYPE,
            data: api.as_ptr() as *mut c_void,
        },
        mpv_render_param {
            type_: MPV_RENDER_PARAM_INVALID,
            data: std::ptr::null_mut(),
        },
    ];
    let r = unsafe { mpv_render_context_create(&mut rctx, h, params.as_mut_ptr()) };
    if r < 0 || rctx.is_null() {
        unsafe {
            mpv_terminate_destroy(h);
            drop(Box::from_raw(user_data));
        }
        return None;
    }
    Some((h, rctx, user_data))
}

// Issue `loadfile dmblob://x replace`.
unsafe fn loadfile(h: *mut mpv_handle) {
    let cmd0 = cstr("loadfile");
    let cmd1 = cstr("dmblob://x");
    let cmd2 = cstr("replace");
    let mut args: [*const c_char; 4] = [
        cmd0.as_ptr(),
        cmd1.as_ptr(),
        cmd2.as_ptr(),
        std::ptr::null(),
    ];
    unsafe {
        mpv_command(h, args.as_mut_ptr());
    }
}

// ─── Live player ─────────────────────────────────────────────────────────────

/// An embedded mpv instance playing one in-memory video, rendering frames to a
/// UI callback and reporting playback state. Drop tears everything down.
pub struct MpvPlayer {
    handle: HandlePtr,
    render: RenderPtr,
    wake: Arc<Wake>,
    wake_raw: *const Wake,
    user_data: *mut Arc<Vec<u8>>,
    render_thread: Option<JoinHandle<()>>,
    event_thread: Option<JoinHandle<()>>,
    paused: Arc<AtomicBool>,
}

// The contained raw pointers are only touched through mpv's thread-safe API.
unsafe impl Send for MpvPlayer {}

impl MpvPlayer {
    /// Start playing `bytes` with audio. `on_frame` receives decoded RGBA
    /// frames (on a worker thread); `on_state` receives playback state on
    /// changes (also off the UI thread). Both callbacks must marshal to the UI
    /// thread themselves (e.g. `slint::invoke_from_event_loop`).
    pub fn open(
        bytes: Vec<u8>,
        max_dim: u32,
        on_frame: impl Fn(PicturePixels) + Send + 'static,
        on_state: impl Fn(PlayerState) + Send + 'static,
    ) -> Option<MpvPlayer> {
        let bytes = Arc::new(bytes);
        let (h, rctx, user_data) = unsafe { build_handle(bytes, true)? };

        let wake = Arc::new(Wake {
            redraw: Mutex::new(false),
            cv: Condvar::new(),
            shutdown: AtomicBool::new(false),
        });
        let wake_raw = Arc::into_raw(wake.clone());
        unsafe {
            mpv_render_context_set_update_callback(
                rctx,
                Some(render_update_cb),
                wake_raw as *mut c_void,
            );
        }

        // Observe playback state.
        unsafe {
            mpv_observe_property(h, 1, c"time-pos".as_ptr(), MPV_FORMAT_DOUBLE);
            mpv_observe_property(h, 2, c"duration".as_ptr(), MPV_FORMAT_DOUBLE);
            mpv_observe_property(h, 3, c"pause".as_ptr(), MPV_FORMAT_FLAG);
            mpv_observe_property(h, 4, c"eof-reached".as_ptr(), MPV_FORMAT_FLAG);
            loadfile(h);
        }

        let handle = HandlePtr(h);
        let render = RenderPtr(rctx);
        let paused = Arc::new(AtomicBool::new(false));

        // Render thread: wake on redraw flag, blit, ship frame.
        let render_thread = {
            let wake = wake.clone();
            std::thread::Builder::new()
                .name("mpv-render".into())
                .spawn(move || {
                    let rctx = render;
                    let h = handle;
                    loop {
                        {
                            let mut g = wake.redraw.lock().unwrap();
                            while !*g && !wake.shutdown.load(Ordering::Acquire) {
                                let (ng, _) = wake
                                    .cv
                                    .wait_timeout(g, std::time::Duration::from_millis(250))
                                    .unwrap();
                                g = ng;
                                if !*g {
                                    break;
                                }
                            }
                            *g = false;
                        }
                        if wake.shutdown.load(Ordering::Acquire) {
                            break;
                        }
                        let flags = unsafe { mpv_render_context_update(rctx.0) };
                        if flags & MPV_RENDER_UPDATE_FRAME != 0
                            && let Some(px) = unsafe { render_frame(rctx.0, h.0, max_dim) }
                        {
                            on_frame(px);
                        }
                    }
                })
                .ok()?
        };

        // Event thread: pump mpv events, report state.
        let event_thread = {
            let wake = wake.clone();
            let paused_c = paused.clone();
            std::thread::Builder::new()
                .name("mpv-event".into())
                .spawn(move || {
                    let h = handle;
                    let mut st = PlayerState::default();
                    loop {
                        if wake.shutdown.load(Ordering::Acquire) {
                            break;
                        }
                        let ev = unsafe { &*mpv_wait_event(h.0, 0.25) };
                        match ev.event_id {
                            MPV_EVENT_NONE => continue,
                            MPV_EVENT_SHUTDOWN => break,
                            MPV_EVENT_END_FILE => {
                                st.eof = true;
                                on_state(st);
                            }
                            MPV_EVENT_PROPERTY_CHANGE => {
                                let prop = unsafe { &*(ev.data as *const mpv_event_property) };
                                if prop.name.is_null() {
                                    continue;
                                }
                                let name = unsafe { CStr::from_ptr(prop.name) };
                                match name.to_bytes() {
                                    b"time-pos" => {
                                        if let Some(v) = unsafe { get_f64(h.0, c"time-pos") } {
                                            st.time_pos = v;
                                        }
                                    }
                                    b"duration" => {
                                        if let Some(v) = unsafe { get_f64(h.0, c"duration") } {
                                            st.duration = v;
                                        }
                                    }
                                    b"pause" => {
                                        if let Some(v) = unsafe { get_flag(h.0, c"pause") } {
                                            st.paused = v;
                                            paused_c.store(v, Ordering::Release);
                                        }
                                    }
                                    b"eof-reached" => {
                                        st.eof = unsafe { get_flag(h.0, c"eof-reached") }
                                            .unwrap_or(false);
                                    }
                                    _ => {}
                                }
                                on_state(st);
                            }
                            _ => {}
                        }
                    }
                })
                .ok()?
        };

        Some(MpvPlayer {
            handle,
            render,
            wake,
            wake_raw,
            user_data,
            render_thread: Some(render_thread),
            event_thread: Some(event_thread),
            paused,
        })
    }

    /// Toggle pause; returns the new paused state.
    pub fn toggle_pause(&self) -> bool {
        let want = !self.paused.load(Ordering::Acquire);
        self.set_paused(want);
        want
    }

    pub fn set_paused(&self, paused: bool) {
        let mut flag: c_int = paused as c_int;
        unsafe {
            mpv_set_property(
                self.handle.0,
                c"pause".as_ptr(),
                MPV_FORMAT_FLAG,
                &mut flag as *mut c_int as *mut c_void,
            );
        }
    }

    /// Seek to an absolute position in seconds.
    pub fn seek(&self, secs: f64) {
        self.seek_cmd(secs, "absolute");
    }

    /// Seek relative to the current position (negative rewinds). mpv clamps to
    /// the clip bounds, so out-of-range steps are harmless.
    pub fn seek_relative(&self, secs: f64) {
        self.seek_cmd(secs, "relative");
    }

    fn seek_cmd(&self, secs: f64, mode: &str) {
        let s = cstr("seek");
        let pos = cstr(&format!("{secs:.3}"));
        let mode = cstr(mode);
        let mut args: [*const c_char; 4] =
            [s.as_ptr(), pos.as_ptr(), mode.as_ptr(), std::ptr::null()];
        unsafe {
            mpv_command(self.handle.0, args.as_mut_ptr());
        }
    }
}

impl Drop for MpvPlayer {
    fn drop(&mut self) {
        // Signal both threads, wake them, and wait.
        self.wake.shutdown.store(true, Ordering::Release);
        *self.wake.redraw.lock().unwrap() = true;
        self.wake.cv.notify_all();
        unsafe { mpv_wakeup(self.handle.0) }; // unblock mpv_wait_event
        if let Some(t) = self.render_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.event_thread.take() {
            let _ = t.join();
        }
        // Threads are gone — now free the context + handle, then the leaked
        // user_data and the callback's Arc.
        unsafe {
            mpv_render_context_set_update_callback(self.render.0, None, std::ptr::null_mut());
            mpv_render_context_free(self.render.0);
            mpv_terminate_destroy(self.handle.0);
            drop(Box::from_raw(self.user_data));
            drop(Arc::from_raw(self.wake_raw));
        }
    }
}
