use crate::*;

// Callback-wiring sections. Each is one logical layer split out only to stay
// under the 2000-line file limit; re-exported so callers see `wire_*` flat.
mod backup;
mod extra;
mod messaging;
mod panes;
pub(crate) use backup::*;
pub(crate) use extra::*;
pub(crate) use messaging::*;
pub(crate) use panes::*;

// Aliases for the handful of boxed-closure / nested-Arc shapes that the wiring
// split now threads through struct fields and function signatures (positions
// clippy's `type_complexity` lints, unlike the local `let` bindings they used
// to live in).
pub(crate) type BackendCell = Arc<Mutex<Option<Arc<Backend>>>>;
pub(crate) type VaultCell = Arc<Mutex<Option<Arc<Mutex<Vault>>>>>;
pub(crate) type SharedFn = Arc<dyn Fn() + Send + Sync>;
pub(crate) type BootFn = Arc<dyn Fn(String, Arc<Mutex<Vault>>, Option<String>) + Send + Sync>;
pub(crate) type DispatchSendFn = Rc<dyn Fn(String, String, String, Option<String>)>;
pub(crate) type EditOpFn = Rc<dyn Fn(String, String)>;

/// Shared, cheaply-cloneable handles that `main()` creates once and every
/// callback-wiring function captures clones of. Bundling them lets each
/// `wire_*` function take a single `&Cx` and reproduce the original local
/// bindings with `let Cx { backend_cell, .. } = cx.clone();` — so the section
/// bodies move out of `main()` verbatim.
#[derive(Clone)]
pub(crate) struct Cx {
    pub(crate) notif: Arc<notify::NotifState>,
    pub(crate) settings_cell: Rc<RefCell<Settings>>,
    pub(crate) contacts: ModelRc<Contact>,
    pub(crate) chats_messages: ModelRc<ModelRc<ChatMessage>>,
    pub(crate) backend_cell: BackendCell,
    pub(crate) vault_cell: VaultCell,
    pub(crate) group_ids: Arc<Mutex<Vec<String>>>,
    pub(crate) archived_group_ids: Arc<Mutex<Vec<String>>>,
    pub(crate) pending_state: Arc<Mutex<PendingState>>,
    pub(crate) staged_files: Arc<Mutex<Vec<StagedFile>>>,
    pub(crate) active_message_watcher: Arc<Mutex<Option<JoinHandle<()>>>>,
    pub(crate) chats_watcher: Arc<Mutex<Option<JoinHandle<()>>>>,
}

/// The cross-section shared closures `main()` builds once up front. They outlive
/// any single `wire_*` chunk (e.g. `dispatch_send` is used by both the messaging
/// and offline-queue wiring), so they're bundled here and threaded alongside
/// `Cx`. A `wire_*` body recreates the bindings it needs with
/// `let Handlers { dispatch_send, .. } = h.clone();`.
#[derive(Clone)]
pub(crate) struct Handlers {
    pub(crate) refresh_breadcrumb: SharedFn,
    pub(crate) refresh_storage_size: SharedFn,
    pub(crate) refresh_all_chat_models: SharedFn,
    pub(crate) dispatch_send: DispatchSendFn,
    pub(crate) edit_op: EditOpFn,
}
