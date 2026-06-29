#![allow(dead_code)] // home(), watch_messages(), save_relays() are wired in the next slice.

// In-process bridge from the Slint UI to marmot-app.
//
// Owns a tokio runtime + MarmotAppRuntime. Exposes blocking helpers the Slint
// event loop can call directly, plus an async subscription pump that forwards
// chat/message updates back to the UI via slint::invoke_from_event_loop.
//
// No daemon, no socket — we link marmot-app directly and play the same role
// `dmd` does in the upstream stack.

pub(crate) use std::collections::{HashMap, HashSet};
pub(crate) use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
pub(crate) use std::path::{Path, PathBuf};
pub(crate) use std::sync::{Arc, Mutex, RwLock};
pub(crate) use std::time::Duration;

pub(crate) use anyhow::{Context, Result, anyhow};
pub(crate) use cgka_traits::GroupId;
pub(crate) use cgka_traits::TransportEndpoint;
pub(crate) use cgka_traits::app_components::BLOSSOM_LOCATOR_KIND_V1;
pub(crate) use marmot_account::{AccountHome, AccountSecretStore, AccountSummary};
pub(crate) use marmot_app::{
    AccountRelayListBootstrap, AccountSetupRequest, AppBlobEndpoint, AppGroupMemberRecord,
    AppGroupMlsState, AppGroupRecord, AppMessageQuery, AppMessageRecord, AuditLogFile,
    AuditLogSettings, AuditLogTrackerConfig, AuditLogUploadSource, DEFAULT_BLOSSOM_SERVER_URL,
    MarmotApp, MarmotAppRuntime, MediaAttachmentReference, MediaDownloadResult,
    MediaUploadAttachmentRequest, MediaUploadRequest, MediaUploadResult, RelayTelemetryResource,
    RelayTelemetryRuntimeConfig, RelayTelemetrySettings, RuntimeMessageUpdate,
    RuntimeMessagesSubscription, SendSummary, UserDirectoryRecord, UserProfileMetadata,
};
pub(crate) use tokio::runtime::Runtime as TokioRuntime;
pub(crate) use tokio::task::JoinHandle;

pub(crate) use crate::observability::ObservabilityConfig;

/// account_id (lowercase) → (display name, picture URL), shared behind a mutex
/// so the background sync and UI-thread reads share one warmed map.
type ProfileCache = Arc<Mutex<HashMap<String, (String, Option<String>)>>>;
/// Boot-progress callback ("Connecting…", "Syncing…"), invoked off the UI thread.
type StatusCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// Default account label used when we bootstrap from a single stored nsec.
pub const DEFAULT_ACCOUNT_LABEL: &str = "default";

/// Well-known relays we *always* consult when discovering a peer's relay list
/// and key package, on top of the user's own configured relays. A peer may have
/// published their NIP-65 list to relays the user doesn't write to (this is the
/// norm — there is no overlap guarantee between two users' relay sets), so
/// resolving members against only the local configured set silently fails to
/// find perfectly reachable peers.
///
/// DEV POLICY: while in development we restrict this to the whitenoise
/// official relays only (the ones the mobile apps publish to). Before release,
/// re-add the broad public indexers (relay.ditto.pub, relay.primal.net,
/// relay.damus.io, nos.lol) so discovery works beyond the whitenoise fleet.
const DISCOVERY_RELAYS: &[&str] = &[
    "wss://relay.eu.whitenoise.chat",
    "wss://relay.us.whitenoise.chat",
];

/// Capture a helper command's stdout, trimmed. `None` on spawn failure or
/// non-zero exit, so callers degrade the same way as an unreadable file.
fn cmd_stdout(cmd: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Kernel `major.minor` (e.g. `7.0`), from `/proc/sys/kernel/osrelease` on
/// Linux and `uname -r` elsewhere (FreeBSD, macOS). The OTLP spec only wants
/// major/minor; we never expose the full patch/build string — release suffixes
/// like FreeBSD's `14.1-RELEASE` are stripped to leading digits. Falls back to
/// `"unknown"` so the required resource attr is non-empty.
fn host_os_version() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .or_else(|| cmd_stdout("uname", &["-r"]))
        .and_then(|s| {
            let digits =
                |p: &str| -> String { p.chars().take_while(|c| c.is_ascii_digit()).collect() };
            let mut parts = s.trim().split('.');
            match (parts.next().map(digits), parts.next().map(digits)) {
                (Some(major), Some(minor)) if !major.is_empty() && !minor.is_empty() => {
                    Some(format!("{major}.{minor}"))
                }
                _ => None,
            }
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// Best-effort hardware model — a coarse, non-user-chosen identifier. Linux
/// reads DMI `product_name` (e.g. `20XW` / `MS-7C91`), macOS asks sysctl for
/// `hw.model` (e.g. `MacBookPro18,3`), FreeBSD reads the same SMBIOS field via
/// `kenv smbios.system.product`. Returns `None` when unreadable or a generic
/// placeholder, since this attr is recommended, not required.
fn host_device_model() -> Option<String> {
    let raw = if cfg!(target_os = "macos") {
        cmd_stdout("sysctl", &["-n", "hw.model"])?
    } else if cfg!(target_os = "freebsd") {
        cmd_stdout("kenv", &["smbios.system.product"])?
    } else {
        std::fs::read_to_string("/sys/class/dmi/id/product_name").ok()?
    };
    let model = raw.trim();
    if model.is_empty()
        || model.eq_ignore_ascii_case("To Be Filled By O.E.M.")
        || model.eq_ignore_ascii_case("System Product Name")
        || model.eq_ignore_ascii_case("Default string")
    {
        return None;
    }
    Some(model.to_string())
}

pub struct Backend {
    tokio: TokioRuntime,
    app: MarmotApp,
    runtime: MarmotAppRuntime,
    account_home: AccountHome,
    /// The account whose chats/contacts/profile the UI is currently showing.
    /// Every account in the home has a running worker (marmot's
    /// `AccountManager` reconciles one per local-signing account), so all
    /// accounts keep receiving in the background — this only selects the
    /// *displayed* one. Swapped by [`Backend::set_active_account`].
    active: RwLock<AccountSummary>,
    home: PathBuf,
    relays: Vec<String>,
    /// Per-group member lists, refreshed asynchronously. UI-thread reads MUST
    /// come from here, never from the account worker queue: worker commands
    /// are FIFO behind long-running catch-up/reconcile, and one misbehaving
    /// relay holds the worker for its full connection timeout (~35s observed)
    /// — which used to freeze every chat switch and the chat-list build.
    members_cache: Arc<Mutex<HashMap<String, Vec<AppGroupMemberRecord>>>>,
    /// Groups with a member refresh currently in flight (dedupe).
    members_inflight: Arc<Mutex<HashSet<String>>>,
    /// account_id (lowercase) → (display name, picture URL). Same rationale
    /// as `members_cache`: directory lookups read marmot's shared storage,
    /// which the always-running background sync writes to — UI-thread reads
    /// were observed blocking 0.1–5s per chat switch under contention.
    /// Warmed at boot, refreshed asynchronously.
    profile_cache: ProfileCache,
    /// Accounts with a profile refresh currently in flight (dedupe).
    profile_inflight: Arc<Mutex<HashSet<String>>>,
}

impl Backend {
    /// Bootstrap the in-process runtime against a previously stored nsec.
    ///
    /// `relays` may be empty — operations that need a relay will fail, but the
    /// runtime still starts so the UI can render an empty state until the user
    /// configures relays.
    /// `on_synced` fires (on a background thread) when the network phase of
    /// boot — directory sync, key-package bootstrap, inbox catch-up —
    /// completes. For an account that already exists locally, `boot` returns
    /// as soon as local storage is open; the network phase can lag by tens of
    /// seconds when a relay misbehaves (auth failure → full connection
    /// timeout), and the UI must not wait on it.
    /// `active_account` is a hint (account-id hex) naming which of the home's
    /// accounts the UI should display first — the last one the user had
    /// active. Falls back to the `nsec`-derived account when absent or no
    /// longer present in the home.
    pub fn boot(
        nsec: &str,
        relays: Vec<String>,
        secret_store: Arc<dyn AccountSecretStore>,
        active_account: Option<String>,
        on_synced: impl FnOnce(Result<()>) + Send + 'static,
        on_status: Option<StatusCallback>,
    ) -> Result<Self> {
        let status = |msg: &str| {
            if let Some(ref cb) = on_status {
                cb(msg);
            }
        };
        let t_boot = std::time::Instant::now();
        status("Opening vault…");
        let home = default_home();
        std::fs::create_dir_all(&home).context("create dm home")?;

        // Account secrets are sealed in the password-encrypted vault (see vault.rs)
        // via this injected store — never libsecret or plaintext JSON.
        let account_home = AccountHome::open_with_secret_store(&home, secret_store);
        let target_id =
            AccountHome::account_id_for_secret(nsec).context("derive account id from nsec")?;

        let tokio = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("build tokio runtime")?;

        let app =
            MarmotApp::with_relays_and_account_home(&home, relays.clone(), account_home.clone());
        let runtime = app.runtime();

        // Whether this identity already exists locally decides the boot
        // shape. An existing account renders from local storage immediately —
        // `runtime.start()` (whose directory sync blocks on relay round-trips
        // and eats any misbehaving relay's full connection timeout) moves to
        // the background; the lifecycle gate only rejects calls while
        // *stopping*, so local snapshot reads work without it. A first run
        // has nothing local to show and must block on the identity-aware
        // import path, which needs a started runtime.
        let already_present = account_home
            .accounts()
            .context("list accounts")?
            .into_iter()
            .any(|a| a.account_id_hex == target_id);
        status("Deriving keys…");
        eprintln!(
            "[boot-timing] local setup done at {:?} (already_present={already_present})",
            t_boot.elapsed()
        );
        if !already_present {
            // Start the runtime. If any existing account record is malformed
            // (e.g. an old account created without the marmot LeafNode
            // identity proof — which earlier versions of this client wrote),
            // start() will fail. We wipe and retry once, then re-import via
            // the proper path.
            Self::start_with_self_heal(&tokio, &runtime, &account_home)?;
            eprintln!(
                "[boot-timing] first-run start done at {:?}",
                t_boot.elapsed()
            );
            Self::login_account(tokio.handle(), &runtime, nsec, &relays)?;
            eprintln!(
                "[boot-timing] first-run login done at {:?}",
                t_boot.elapsed()
            );
        }

        // Resolve the account the UI will display first. Every account in
        // the home gets a running worker regardless; this only picks the
        // initial view. Prefer the caller's last-active hint when that
        // account still exists, otherwise the nsec-derived one.
        let all_accounts = account_home
            .accounts()
            .context("list accounts after login")?;
        let account = all_accounts
            .iter()
            .find(|a| a.account_id_hex == target_id)
            .cloned()
            .ok_or_else(|| anyhow!("account did not appear in home after login"))?;
        let account = active_account
            .and_then(|hint| {
                all_accounts
                    .iter()
                    .find(|a| a.account_id_hex.eq_ignore_ascii_case(&hint))
                    .cloned()
            })
            .unwrap_or(account);

        // A low-frequency background poll so invites that arrive while
        // the app is running show up without requiring user action. 15s is
        // arbitrary — picks up new welcomes quickly without hammering relays.
        {
            let rt = runtime.clone();
            tokio.spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
                tick.tick().await; // first tick fires immediately; skip it
                loop {
                    tick.tick().await;
                    if let Err(e) = rt.catch_up_accounts().await {
                        eprintln!("[backend] periodic catch_up_accounts failed: {e}");
                    }
                }
            });
        }

        let backend = Self {
            tokio,
            app,
            runtime,
            account_home,
            active: RwLock::new(account),
            home,
            relays,
            members_cache: Arc::new(Mutex::new(HashMap::new())),
            members_inflight: Arc::new(Mutex::new(HashSet::new())),
            profile_cache: Arc::new(Mutex::new(HashMap::new())),
            profile_inflight: Arc::new(Mutex::new(HashSet::new())),
        };

        // Warm the members + profile caches NOW, before the background
        // network phase starts competing for the account worker and the
        // shared directory storage: these local reads land in milliseconds
        // here but block behind relay timeouts afterwards.
        backend.warm_members_cache();
        backend.warm_profile_cache();

        status("Publishing key package…");

        // Point marmot's telemetry exporter + audit-log tracker at the IPF
        // services. The library auto-exports metrics (~60s) and auto-uploads
        // audit logs after sends/syncs — we only supply endpoints, tokens, and
        // resource attributes. Failures are non-fatal: they just mean no
        // telemetry/audit until the next boot. Configured before the
        // (possibly background) runtime start so start() reads the real
        // endpoints when it sets up the exporter.
        backend.configure_observability();

        // An already-present account skipped the synchronous start above, so
        // the background phase begins with runtime.start().
        backend.spawn_background_sync(
            nsec.to_string(),
            /* needs_start */ already_present,
            on_synced,
        );

        status("Connecting to relays…");
        eprintln!("[boot-timing] boot returning at {:?}", t_boot.elapsed());
        Ok(backend)
    }

    /// The network phase of boot, off the UI's critical path:
    ///
    /// 1. For an existing account (`needs_start`), `runtime.start()` itself —
    ///    its directory sync blocks on relay round-trips, and a misbehaving
    ///    relay holds it for a full connection timeout (~35s observed on a
    ///    relay rejecting auth). Self-heal mirrors [`Self::start_with_self_heal`],
    ///    plus a re-import since the wipe removes our account.
    /// 2. KP bootstrap: if the account has no locally-recorded key package
    ///    and we have relays to publish to, publish one. Without this, peers
    ///    can't find a fresh KP to invite us with (and any KP they cached
    ///    from before the local state was wiped is stale → silently
    ///    unpeelable welcomes).
    /// 3. Inbox catch-up: pull anything that landed while we were closed —
    ///    welcomes from peers, group evolutions, etc. `runtime.start()` only
    ///    does directory sync + reconcile; it does NOT poll the inbox relay.
    ///
    /// `on_synced` fires (still on the background thread) when the phase
    /// ends so the UI can do one refresh to pick up whatever it pulled in.
    fn spawn_background_sync(
        &self,
        nsec: String,
        needs_start: bool,
        on_synced: impl FnOnce(Result<()>) + Send + 'static,
    ) {
        let handle = self.tokio.handle().clone();
        let runtime = self.runtime.clone();
        let account_home = self.account_home.clone();
        let app = self.app.clone();
        let relays = self.relays.clone();
        let label = self.active_label();
        std::thread::spawn(move || {
            let t_sync = std::time::Instant::now();
            let result = (|| -> Result<()> {
                if needs_start {
                    let rt = runtime.clone();
                    let first = handle.block_on(async move { rt.start().await });
                    eprintln!(
                        "[boot-timing] background runtime.start done at {:?} (ok={})",
                        t_sync.elapsed(),
                        first.is_ok()
                    );
                    if let Err(err) = first {
                        eprintln!(
                            "[backend] runtime.start failed ({err}); wiping local accounts and retrying"
                        );
                        for acc in account_home.accounts().unwrap_or_default() {
                            if let Err(e) = account_home.remove_account(&acc.label) {
                                eprintln!("[backend] remove_account({}) failed: {e}", acc.label);
                            }
                        }
                        let rt = runtime.clone();
                        handle
                            .block_on(async move { rt.start().await })
                            .context("runtime.start retry after wipe")?;
                        // The wipe removed our account — re-import it. The
                        // account id is derived from the nsec, so the summary
                        // the Backend resolved at boot stays valid.
                        Self::login_account(&handle, &runtime, &nsec, &relays)?;
                    }
                }
                let has_kp = app
                    .local_key_package_records(&label)
                    .map(|v| !v.is_empty())
                    .unwrap_or(false);
                if !relays.is_empty() && !has_kp {
                    let rt = runtime.clone();
                    let l = label.clone();
                    match handle.block_on(async move { rt.publish_key_package(&l).await }) {
                        Ok(acks) => eprintln!(
                            "[backend] bootstrap-published key package ({acks} relay acks)"
                        ),
                        Err(e) => {
                            eprintln!("[backend] bootstrap publish_key_package failed: {e}")
                        }
                    }
                }
                let rt = runtime.clone();
                if let Err(e) = handle.block_on(async move { rt.catch_up_accounts().await }) {
                    eprintln!("[backend] initial catch_up_accounts failed: {e}");
                }
                eprintln!(
                    "[boot-timing] background sync finished at {:?}",
                    t_sync.elapsed()
                );
                Ok(())
            })();
            on_synced(result);
        });
    }

    /// Configure marmot's relay-telemetry exporter (OTLP/HTTP metrics) and the
    /// audit-log tracker (Goggles NDJSON upload). Idempotent; safe to call once
    /// per boot after the runtime is running. Whether anything is actually sent
    /// still depends on the user's telemetry/audit *enabled* toggles.
    fn configure_observability(&self) {
        // marmot's telemetry setter spawns its exporter task with a bare
        // `tokio::spawn` (runtime.rs), which panics ("no reactor running")
        // unless a runtime is the ambient context. `boot` calls us on the
        // caller's thread, *outside* `self.tokio`, so enter it for the
        // duration of these setters. (Manifested only on first-run/new-user
        // boot, where the exporter actually gets constructed.)
        let _rt_guard = self.tokio.enter();

        // Endpoints + tokens come from `observability.toml` (embedded default,
        // overridable at `$DM_HOME/observability.toml`).
        let cfg = ObservabilityConfig::load(&self.home);

        // Stable per-install UUID, persisted by marmot in shared storage.
        let install_id = self
            .app
            .telemetry_install_id()
            .unwrap_or_else(|_| "00000000-0000-0000-0000-000000000000".to_string());

        let resource = RelayTelemetryResource {
            service_version: env!("CARGO_PKG_VERSION").to_string(),
            service_instance_id: install_id,
            deployment_environment: cfg.deployment_environment.clone(),
            tenant: cfg.tenant.clone(),
            os_type: "linux".to_string(),
            os_version: host_os_version(),
            device_model_identifier: host_device_model(),
        };

        if let Err(e) =
            self.runtime
                .set_relay_telemetry_runtime_config(RelayTelemetryRuntimeConfig {
                    otlp_endpoint: Some(cfg.otlp_metrics_endpoint.clone()),
                    authorization_bearer_token: Some(cfg.otlp_token.clone()),
                    resource: Some(resource),
                })
        {
            eprintln!("[backend] telemetry runtime config rejected: {e}");
        }

        if let Err(e) = self
            .runtime
            .set_audit_log_tracker_config(AuditLogTrackerConfig {
                endpoint: Some(cfg.goggles_audit_endpoint.clone()),
                authorization_bearer_token: Some(cfg.goggles_token.clone()),
                source: AuditLogUploadSource {
                    device_label: Some(self.active_label()),
                    platform: Some("linux".to_string()),
                    app_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                },
            })
        {
            eprintln!("[backend] audit-log tracker config rejected: {e}");
        }
    }

    /// Try `runtime.start()`. If it fails with a malformed-account error from
    /// an earlier buggy import path, remove ALL local accounts and retry.
    ///
    /// This is a one-shot migration: per-account state (group records, etc.)
    /// is sacrificed. Acceptable because the data was unusable anyway.
    fn start_with_self_heal(
        tokio: &TokioRuntime,
        runtime: &MarmotAppRuntime,
        account_home: &AccountHome,
    ) -> Result<()> {
        let rt = runtime.clone();
        let first = tokio.block_on(async move { rt.start().await });
        if first.is_ok() {
            return Ok(());
        }
        let err = first.err().unwrap();
        eprintln!("[backend] runtime.start failed ({err}); wiping local accounts and retrying");
        for acc in account_home.accounts().unwrap_or_default() {
            if let Err(e) = account_home.remove_account(&acc.label) {
                eprintln!("[backend] remove_account({}) failed: {e}", acc.label);
            }
        }
        let rt = runtime.clone();
        tokio
            .block_on(async move { rt.start().await })
            .context("runtime.start retry after wipe")
    }

    /// Import (or re-import) an account using the runtime's identity-aware
    /// path. This is what writes the marmot `account-identity-proof v1`
    /// LeafNode extension that `runtime.start()` validates.
    fn login_account(
        tokio: &tokio::runtime::Handle,
        runtime: &MarmotAppRuntime,
        nsec: &str,
        relays: &[String],
    ) -> Result<()> {
        let endpoints: Vec<TransportEndpoint> = relays
            .iter()
            .cloned()
            .map(TransportEndpoint::from)
            .collect();
        let request = AccountSetupRequest {
            identity: None, // runtime.login() fills this from the `identity` arg
            default_relays: endpoints.clone(),
            bootstrap_relays: endpoints,
            // Only attempt relay-list / key-package publishing when we have
            // somewhere to publish to. Otherwise the login round-trip times
            // out instead of giving the user a working local identity.
            // (login also validates the lists against the relays, so a
            // brand-new identity hard-fails with MissingRelayLists when
            // publishing is disabled — this can't move to the background.)
            publish_missing_relay_lists: !relays.is_empty(),
            publish_initial_key_package: !relays.is_empty(),
        };
        let nsec = nsec.to_string();
        let runtime_for_login = runtime.clone();
        tokio
            .block_on(async move { runtime_for_login.login(nsec, request).await })
            .context("runtime.login")?;
        Ok(())
    }

    /// Snapshot of the currently-displayed account.
    pub fn account(&self) -> AccountSummary {
        self.active.read().unwrap().clone()
    }

    /// Label of the active account (the key every marmot runtime API takes).
    fn active_label(&self) -> String {
        self.active.read().unwrap().label.clone()
    }

    /// Account-id hex of the active account.
    fn active_id(&self) -> String {
        self.active.read().unwrap().account_id_hex.clone()
    }

    /// All local-signing accounts in the home, in storage order. Every one of
    /// these has (or is getting) a running background worker.
    pub fn accounts(&self) -> Vec<AccountSummary> {
        self.account_home
            .accounts()
            .unwrap_or_default()
            .into_iter()
            .filter(|a| a.local_signing)
            .collect()
    }

    /// Switch the displayed account. Cheap and synchronous: swaps the active
    /// summary and drops the members/profile caches (they encode the previous
    /// account's perspective — group membership and the "You" self-name), then
    /// queues an async re-warm. The UI layer is responsible for rebuilding its
    /// models and re-subscribing its watchers afterwards.
    pub fn set_active_account(&self, account_id_hex: &str) -> Result<AccountSummary> {
        let summary = self
            .accounts()
            .into_iter()
            .find(|a| a.account_id_hex.eq_ignore_ascii_case(account_id_hex))
            .ok_or_else(|| anyhow!("no account {account_id_hex} in the home"))?;
        *self.active.write().unwrap() = summary.clone();
        self.members_cache.lock().unwrap().clear();
        self.profile_cache.lock().unwrap().clear();
        self.rewarm_caches_async();
        Ok(summary)
    }

    /// Background refill of the members + profile caches for the active
    /// account — the async sibling of the synchronous boot-time warmers.
    /// Until it lands, cache misses fall back to hex tails and queue their
    /// own per-entry refreshes, so this is convergence-speed, not correctness.
    fn rewarm_caches_async(&self) {
        let app = self.app.clone();
        let runtime = self.runtime.clone();
        let label = self.active_label();
        let me = self.active_id();
        let members_cache = self.members_cache.clone();
        let profile_cache = self.profile_cache.clone();
        self.tokio.spawn(async move {
            let groups = app.groups(&label).unwrap_or_default();
            let mut ids: HashSet<String> = HashSet::new();
            ids.insert(me.to_ascii_lowercase());
            for g in &groups {
                let Ok(group_id) = group_id_from_hex(&g.group_id_hex) else {
                    continue;
                };
                match runtime.group_members(&label, &group_id).await {
                    Ok(members) => {
                        for m in &members {
                            ids.insert(m.member_id_hex.to_ascii_lowercase());
                        }
                        members_cache
                            .lock()
                            .unwrap()
                            .insert(g.group_id_hex.clone(), members);
                    }
                    Err(e) => {
                        eprintln!("[backend] rewarm members ({}) failed: {e}", g.group_id_hex)
                    }
                }
            }
            for id in ids {
                let v = Self::name_and_picture_direct(&app, &me, &id);
                profile_cache.lock().unwrap().insert(id, v);
            }
        });
    }

    /// Import another account into the *running* runtime and start its
    /// worker — marmot's `create_or_import_account` ends with a reconcile, so
    /// the new account begins receiving immediately, no restart needed.
    /// Non-blocking: the login round-trip (relay-list + key-package publish)
    /// runs on the tokio runtime; `on_done` fires on a worker thread with the
    /// new account's summary. Does NOT change the active account.
    pub fn add_account_async<F>(&self, nsec: String, on_done: F)
    where
        F: FnOnce(Result<AccountSummary>) + Send + 'static,
    {
        let target_id = match AccountHome::account_id_for_secret(&nsec) {
            Ok(id) => id,
            Err(e) => {
                on_done(Err(anyhow!("derive account id from nsec: {e}")));
                return;
            }
        };
        if self
            .accounts()
            .iter()
            .any(|a| a.account_id_hex.eq_ignore_ascii_case(&target_id))
        {
            on_done(Err(anyhow!("that account is already added")));
            return;
        }
        let endpoints: Vec<TransportEndpoint> = self
            .relays
            .iter()
            .cloned()
            .map(TransportEndpoint::from)
            .collect();
        let request = AccountSetupRequest {
            identity: None, // runtime.login() fills this from the nsec
            default_relays: endpoints.clone(),
            bootstrap_relays: endpoints,
            // Same rationale as login_account: only publish when there is
            // somewhere to publish to.
            publish_missing_relay_lists: !self.relays.is_empty(),
            publish_initial_key_package: !self.relays.is_empty(),
        };
        let runtime = self.runtime.clone();
        let account_home = self.account_home.clone();
        self.tokio.spawn(async move {
            let result = match runtime.login(nsec, request).await {
                Ok(_) => account_home
                    .accounts()
                    .context("list accounts after login")
                    .and_then(|accounts| {
                        accounts
                            .into_iter()
                            .find(|a| a.account_id_hex.eq_ignore_ascii_case(&target_id))
                            .ok_or_else(|| anyhow!("account did not appear in home after login"))
                    }),
                Err(e) => Err(anyhow!("login: {e}")),
            };
            on_done(result);
        });
    }

    /// Handle to the in-process tokio runtime, so callers can spawn their own
    /// background tasks (e.g. fetching a profile picture over HTTP) without
    /// having to create a second runtime.
    pub fn tokio_handle(&self) -> tokio::runtime::Handle {
        self.tokio.handle().clone()
    }

    pub fn home(&self) -> &PathBuf {
        &self.home
    }

    /// Snapshot of visible (non-archived) chats for the active account.
    pub fn chats(&self) -> Result<Vec<AppGroupRecord>> {
        // Direct snapshot read. Don't go through subscribe_chats here: it
        // spawns a live-update forwarder task and a broadcast subscriber per
        // call that we'd immediately throw away.
        self.app
            .visible_groups(&self.active_label())
            .map_err(|e| anyhow!("visible_groups: {e}"))
    }

    /// Snapshot of archived chats for the active account.
    pub fn archived_chats(&self) -> Result<Vec<AppGroupRecord>> {
        Ok(self
            .app
            .groups(&self.active_label())
            .map_err(|e| anyhow!("groups: {e}"))?
            .into_iter()
            .filter(|g| g.archived)
            .collect())
    }

    /// Most recent **user-visible** message in a group, if any. Pulls a small
    /// recent window and returns the newest record whose kind is a normal
    /// chat (9). Push-token gossip (MIP-05 kinds 447/448/449), reactions,
    /// deletes, etc. are skipped so chat-list previews stay clean.
    pub fn latest_message(&self, group_hex: &str) -> Option<AppMessageRecord> {
        let query = AppMessageQuery {
            group_id_hex: Some(group_hex.to_string()),
            limit: Some(32),
        };
        let mut snapshot = self
            .runtime
            .messages_with_query(&self.active_label(), query)
            .ok()?;
        // snapshot is oldest-first; walk back to find the most recent visible
        // entry.
        while let Some(record) = snapshot.pop() {
            if record.kind == 9 && !record.plaintext.trim_start().starts_with(r#"{"v":"mip05"#) {
                return Some(record);
            }
        }
        None
    }

    /// Read the local account's currently-known profile metadata. Returns
    /// `Ok(None)` when the directory cache hasn't seen a profile event yet
    /// (typical for first launch with no relays configured).
    pub fn load_profile(&self) -> Result<Option<UserProfileMetadata>> {
        let entry = self
            .app
            .directory_entry_for_account_id(&self.active_id())
            .map_err(|e| anyhow!("directory_entry: {e}"))?;
        Ok(entry.and_then(|e| e.profile))
    }

    /// Publish a new profile (Nostr kind-0 metadata event) and remember it in
    /// the directory cache. Requires at least one relay to be configured.
    pub fn save_profile(&self, profile: UserProfileMetadata) -> Result<UserProfileMetadata> {
        self.save_profile_for_label(&self.active_label(), profile)
    }

    /// Like [`Backend::save_profile`] but for an explicit account label —
    /// used to seed a starter profile on a freshly generated account that
    /// isn't (yet) the active one.
    pub fn save_profile_for_label(
        &self,
        label: &str,
        profile: UserProfileMetadata,
    ) -> Result<UserProfileMetadata> {
        if self.relays.is_empty() {
            return Err(anyhow!(
                "no relays configured — set ~/.config/darkmatter-linux/relays.json first"
            ));
        }
        let endpoints: Vec<TransportEndpoint> = self
            .relays
            .iter()
            .cloned()
            .map(TransportEndpoint::from)
            .collect();
        let bootstrap = AccountRelayListBootstrap::new(endpoints.clone(), endpoints);
        let label = label.to_string();
        let runtime = self.runtime.clone();
        self.tokio.block_on(async move {
            runtime
                .publish_user_profile(&label, profile, bootstrap)
                .await
                .map_err(|e| anyhow!("publish_user_profile: {e}"))
        })
    }

    /// Upload an image to Blossom as a *public* blob and hand the resulting URL
    /// back through `on_done`. Used to host profile pictures: the URL is what we
    /// then store in the kind-0 `picture` field via [`Backend::save_profile`].
    ///
    /// Non-blocking — the upload runs on the tokio runtime and `on_done` fires
    /// on a worker thread (callers bounce back to the UI via
    /// `slint::invoke_from_event_loop`). Signs the Blossom auth event with the
    /// account's own keys so the upload is attributable to this pubkey.
    pub fn upload_public_blob_async<F>(&self, bytes: Vec<u8>, content_type: String, on_done: F)
    where
        F: FnOnce(Result<String>) + Send + 'static,
    {
        self.upload_public_blob_for_label_async(&self.active_label(), bytes, content_type, on_done)
    }

    /// Like [`Backend::upload_public_blob_async`] but signs with an explicit
    /// account label — used to host the seeded starter avatar for a freshly
    /// generated account that isn't (yet) the active one.
    pub fn upload_public_blob_for_label_async<F>(
        &self,
        label: &str,
        bytes: Vec<u8>,
        content_type: String,
        on_done: F,
    ) where
        F: FnOnce(Result<String>) + Send + 'static,
    {
        let keys = match self.account_home.load_signing_keys(label) {
            Ok(keys) => keys,
            Err(e) => {
                on_done(Err(anyhow!("load signing keys: {e}")));
                return;
            }
        };
        let server = crate::blossom::DEFAULT_BLOSSOM_SERVER.to_string();
        self.tokio.spawn(async move {
            let result =
                crate::blossom::upload_public_blob(&server, bytes, &content_type, &keys).await;
            on_done(result);
        });
    }

    /// Read the local account's follow list as a list of directory records.
    /// Falls back to empty when the directory cache hasn't been populated yet.
    pub fn follow_list(&self) -> Result<Vec<UserDirectoryRecord>> {
        let me = match self
            .app
            .directory_entry_for_account_id(&self.active_id())
            .map_err(|e| anyhow!("directory_entry: {e}"))?
        {
            Some(entry) => entry,
            None => return Ok(Vec::new()),
        };
        let mut out = Vec::with_capacity(me.follows.len());
        for follow_id in &me.follows {
            match self.app.directory_entry_for_account_id(follow_id) {
                Ok(Some(entry)) => out.push(entry),
                Ok(None) => out.push(stub_directory_entry(follow_id)),
                Err(_) => out.push(stub_directory_entry(follow_id)),
            }
        }
        Ok(out)
    }

    /// Follow a new contact: append them to the account's kind-3 follow list,
    /// republish it, then re-sync the directory so [`Backend::follow_list`]
    /// reflects the change immediately. Accepts npub or hex; returns the
    /// contact's account id (hex) so the caller can select the new row.
    pub fn add_contact(&self, who: &str) -> Result<String> {
        let account_id_hex = nostr::PublicKey::parse(who)
            .map_err(|_| anyhow!("not a valid npub or hex pubkey"))?
            .to_hex();
        if account_id_hex.eq_ignore_ascii_case(&self.active_id()) {
            return Err(anyhow!("that's your own key"));
        }
        let mut follows = self
            .app
            .directory_entry_for_account_id(&self.active_id())
            .map_err(|e| anyhow!("directory_entry: {e}"))?
            .map(|e| e.follows)
            .unwrap_or_default();
        if follows
            .iter()
            .any(|f| f.eq_ignore_ascii_case(&account_id_hex))
        {
            return Err(anyhow!("already in your contacts"));
        }
        follows.push(account_id_hex.clone());

        if self.relays.is_empty() {
            return Err(anyhow!(
                "no relays configured — set ~/.config/darkmatter-linux/relays.json first"
            ));
        }
        let endpoints: Vec<TransportEndpoint> = self
            .relays
            .iter()
            .cloned()
            .map(TransportEndpoint::from)
            .collect();
        let bootstrap = AccountRelayListBootstrap::new(endpoints.clone(), endpoints);
        let label = self.active_label();
        let runtime = self.runtime.clone();
        self.tokio.block_on(async move {
            runtime
                .publish_account_follow_list(&label, &follows, bootstrap)
                .await
                .map_err(|e| anyhow!("publish_follow_list: {e}"))
        })?;

        // Publishing doesn't touch the directory cache — re-sync from the broad
        // discovery set so the sidebar updates now and the peer's profile/relay
        // lists (possibly only on the whitenoise discovery relays) get cached.
        let app = self.app.clone();
        let me = self.active_id();
        let broad = self.discovery_relays();
        self.tokio
            .block_on(async move { app.refresh_user_directory_for_account_id(&me, broad).await })
            .map_err(|e| anyhow!("refresh_directory: {e}"))?;
        Ok(account_id_hex)
    }

    /// Snapshot of messages for a group, newest-last.
    pub fn messages(&self, group_hex: &str, limit: Option<usize>) -> Result<Vec<AppMessageRecord>> {
        // Direct snapshot read — no subscription. This runs on the UI thread
        // for every chat switch and every surgical row refresh, so it must not
        // pay for a forwarder task + full-history dedup set per call.
        let query = AppMessageQuery {
            group_id_hex: Some(group_hex.to_string()),
            limit,
        };
        self.runtime
            .messages_with_query(&self.active_label(), query)
            .map_err(|e| anyhow!("messages_with_query: {e}"))
    }

    /// Synchronously send a text message — blocks the UI thread for the
    /// duration of the network round-trip. Acceptable for the v1 wiring;
    /// move to spawn + callback once we want a real busy indicator.
    pub fn send_text(&self, group_hex: &str, text: &str) -> Result<SendSummary> {
        let bytes = hex::decode(group_hex).context("decode group id")?;
        let group_id = GroupId::new(bytes);
        let label = self.active_label();
        let runtime = self.runtime.clone();
        let payload = text.as_bytes().to_vec();
        eprintln!(
            "[send] -> group={} label={} len={}",
            group_hex,
            label,
            payload.len()
        );
        let result = self.tokio.block_on(async move {
            runtime
                .send_message(&label, &group_id, payload)
                .await
                .map_err(|e| anyhow!("send_message: {e}"))
        });
        match &result {
            Ok(summary) => eprintln!(
                "[send] <- ok published={} ids={:?}",
                summary.published, summary.message_ids
            ),
            Err(e) => eprintln!("[send] <- err {e:#}"),
        }
        result
    }

    /// Non-blocking send: dispatches the network round-trip onto the tokio
    /// runtime and returns immediately. The callback fires (on a tokio worker
    /// thread) when the send resolves. The UI is responsible for hopping back
    /// onto the Slint event loop in the callback.
    ///
    /// This is the engine behind optimistic-rendering — the UI inserts a
    /// pending bubble first, then calls this, then reconciles on done.
    pub fn send_text_async<F>(&self, group_hex: &str, text: &str, on_done: F)
    where
        F: FnOnce(Result<SendSummary>) + Send + 'static,
    {
        let group_id = match group_id_from_hex(group_hex) {
            Ok(g) => g,
            Err(e) => {
                on_done(Err(e));
                return;
            }
        };
        let label = self.active_label();
        let runtime = self.runtime.clone();
        let payload = text.as_bytes().to_vec();
        self.tokio.spawn(async move {
            let res = runtime
                .send_message(&label, &group_id, payload)
                .await
                .map_err(|e| anyhow!("send_message: {e}"));
            on_done(res);
        });
    }

    /// Non-blocking media upload + send. Encrypts `plaintext` with the
    /// group's MLS exporter secret, uploads the encrypted blob to Blossom,
    /// and publishes a kind-9 chat carrying the NIP-92 `imeta` tag in one
    /// flow. `on_done` fires on the tokio runtime once the round-trip
    /// resolves.
    pub fn upload_media_async<F>(
        &self,
        group_hex: &str,
        file_name: String,
        media_type: String,
        plaintext: Vec<u8>,
        caption: Option<String>,
        on_done: F,
    ) where
        F: FnOnce(Result<MediaUploadResult>) + Send + 'static,
    {
        let group_id = match group_id_from_hex(group_hex) {
            Ok(g) => g,
            Err(e) => {
                on_done(Err(e));
                return;
            }
        };
        let label = self.active_label();
        let runtime = self.runtime.clone();
        let request = MediaUploadRequest {
            attachments: vec![MediaUploadAttachmentRequest {
                file_name,
                media_type,
                plaintext,
                dim: None,
                thumbhash: None,
            }],
            caption,
            send: true,
            blossom_server: None,
        };
        self.tokio.spawn(async move {
            on_done(upload_media_with_heal(runtime, label, group_id, request).await);
        });
    }

    /// Non-blocking album upload + send: all images go out as **one** kind-9
    /// message carrying one `imeta` tag per image (so the UI renders them as a
    /// single grid bubble). Each item is `(file_name, media_type, plaintext,
    /// dim)`, where `dim` is `"WxH"` so receivers can lay out the grid without
    /// decoding. Shares the same self-heal-and-retry as [`upload_media_async`].
    pub fn upload_album_async<F>(
        &self,
        group_hex: &str,
        items: Vec<(String, String, Vec<u8>, Option<String>)>,
        on_done: F,
    ) where
        F: FnOnce(Result<MediaUploadResult>) + Send + 'static,
    {
        let group_id = match group_id_from_hex(group_hex) {
            Ok(g) => g,
            Err(e) => {
                on_done(Err(e));
                return;
            }
        };
        let label = self.active_label();
        let runtime = self.runtime.clone();
        let request = MediaUploadRequest {
            attachments: items
                .into_iter()
                .map(
                    |(file_name, media_type, plaintext, dim)| MediaUploadAttachmentRequest {
                        file_name,
                        media_type,
                        plaintext,
                        dim,
                        thumbhash: None,
                    },
                )
                .collect(),
            caption: None,
            send: true,
            blossom_server: None,
        };
        self.tokio.spawn(async move {
            on_done(upload_media_with_heal(runtime, label, group_id, request).await);
        });
    }

    /// Non-blocking media download + decrypt. Fetches the encrypted blob
    /// from Blossom, verifies the ciphertext hash, decrypts with the
    /// group's exporter secret, and hands back the plaintext bytes + the
    /// resolved filename/mime/size.
    pub fn download_media_async<F>(
        &self,
        group_hex: &str,
        reference: MediaAttachmentReference,
        on_done: F,
    ) where
        F: FnOnce(Result<MediaDownloadResult>) + Send + 'static,
    {
        let group_id = match group_id_from_hex(group_hex) {
            Ok(g) => g,
            Err(e) => {
                on_done(Err(e));
                return;
            }
        };
        let label = self.active_label();
        let runtime = self.runtime.clone();
        self.tokio.spawn(async move {
            let res = download_media_with_redirect_retry(runtime, label, group_id, reference).await;
            on_done(res);
        });
    }

    /// Non-blocking reply send. Same shape as [`send_text_async`] — the
    /// difference is the wire event carries `e` + `q` tags pointing at
    /// `parent_message_id_hex`, encoded by `AppMessageIntent::Reply`. The
    /// optimistic-render reconciliation in the UI layer treats it identically
    /// to a normal send (it's still a kind-9 chat).
    pub fn reply_text_async<F>(
        &self,
        group_hex: &str,
        parent_message_id_hex: &str,
        text: &str,
        on_done: F,
    ) where
        F: FnOnce(Result<SendSummary>) + Send + 'static,
    {
        let group_id = match group_id_from_hex(group_hex) {
            Ok(g) => g,
            Err(e) => {
                on_done(Err(e));
                return;
            }
        };
        let label = self.active_label();
        let runtime = self.runtime.clone();
        let parent = parent_message_id_hex.to_string();
        let text = text.to_string();
        self.tokio.spawn(async move {
            let res = runtime
                .reply_to_message(&label, &group_id, &parent, &text)
                .await
                .map_err(|e| anyhow!("reply_to_message: {e}"));
            on_done(res);
        });
    }

    /// Non-blocking variant of [`react`]. See [`send_text_async`] for the
    /// rationale — same shape, optimistic-render reconciliation lives in the
    /// UI layer.
    pub fn react_async<F>(&self, group_hex: &str, message_id_hex: &str, emoji: &str, on_done: F)
    where
        F: FnOnce(Result<SendSummary>) + Send + 'static,
    {
        let group_id = match group_id_from_hex(group_hex) {
            Ok(g) => g,
            Err(e) => {
                on_done(Err(e));
                return;
            }
        };
        let label = self.active_label();
        let runtime = self.runtime.clone();
        let target = message_id_hex.to_string();
        let emoji = emoji.to_string();
        self.tokio.spawn(async move {
            let res = runtime
                .react_to_message(&label, &group_id, &target, &emoji)
                .await
                .map_err(|e| anyhow!("react_to_message: {e}"));
            on_done(res);
        });
    }

    /// Publish a kind-1009 edit of `message_id_hex` with replacement text
    /// `content`. Same optimistic-reconciliation shape as [`react_async`] — the
    /// UI overlay rewrites the bubble immediately and the ack/echo reconciles.
    pub fn edit_message_async<F>(
        &self,
        group_hex: &str,
        message_id_hex: &str,
        content: &str,
        on_done: F,
    ) where
        F: FnOnce(Result<SendSummary>) + Send + 'static,
    {
        let group_id = match group_id_from_hex(group_hex) {
            Ok(g) => g,
            Err(e) => {
                on_done(Err(e));
                return;
            }
        };
        let label = self.active_label();
        let runtime = self.runtime.clone();
        let target = message_id_hex.to_string();
        let content = content.to_string();
        self.tokio.spawn(async move {
            let res = runtime
                .edit_message(&label, &group_id, &target, &content)
                .await
                .map_err(|e| anyhow!("edit_message: {e}"));
            on_done(res);
        });
    }

    /// Retract `message_id_hex` for everyone: publish a kind-5 delete event
    /// referencing the target. Marmot enforces on read that a delete is only
    /// honored when its authenticated author matches the target's author, so
    /// this only meaningfully retracts the user's own messages. Same optimistic-
    /// reconciliation shape as [`edit_message_async`].
    pub fn delete_message_async<F>(&self, group_hex: &str, message_id_hex: &str, on_done: F)
    where
        F: FnOnce(Result<SendSummary>) + Send + 'static,
    {
        let group_id = match group_id_from_hex(group_hex) {
            Ok(g) => g,
            Err(e) => {
                on_done(Err(e));
                return;
            }
        };
        let label = self.active_label();
        let runtime = self.runtime.clone();
        let target = message_id_hex.to_string();
        self.tokio.spawn(async move {
            let res = runtime
                .delete_message(&label, &group_id, &target)
                .await
                .map_err(|e| anyhow!("delete_message: {e}"));
            on_done(res);
        });
    }

    /// Non-blocking variant of [`unreact`].
    pub fn unreact_async<F>(&self, group_hex: &str, message_id_hex: &str, on_done: F)
    where
        F: FnOnce(Result<SendSummary>) + Send + 'static,
    {
        let group_id = match group_id_from_hex(group_hex) {
            Ok(g) => g,
            Err(e) => {
                on_done(Err(e));
                return;
            }
        };
        let label = self.active_label();
        let runtime = self.runtime.clone();
        let target = message_id_hex.to_string();
        self.tokio.spawn(async move {
            let res = runtime
                .unreact_from_message(&label, &group_id, &target)
                .await
                .map_err(|e| anyhow!("unreact_from_message: {e}"));
            on_done(res);
        });
    }

    /// Add a reaction (`emoji`) to a message in `group_hex`.
    pub fn react(&self, group_hex: &str, message_id_hex: &str, emoji: &str) -> Result<SendSummary> {
        let group_id = group_id_from_hex(group_hex)?;
        let label = self.active_label();
        let runtime = self.runtime.clone();
        let target = message_id_hex.to_string();
        let emoji = emoji.to_string();
        self.tokio.block_on(async move {
            runtime
                .react_to_message(&label, &group_id, &target, &emoji)
                .await
                .map_err(|e| anyhow!("react_to_message: {e}"))
        })
    }

    /// Remove **all** of my reactions from a message (marmot-app semantics —
    /// there's no per-emoji unreact, just a blanket clear).
    pub fn unreact(&self, group_hex: &str, message_id_hex: &str) -> Result<SendSummary> {
        let group_id = group_id_from_hex(group_hex)?;
        let label = self.active_label();
        let runtime = self.runtime.clone();
        let target = message_id_hex.to_string();
        self.tokio.block_on(async move {
            runtime
                .unreact_from_message(&label, &group_id, &target)
                .await
                .map_err(|e| anyhow!("unreact_from_message: {e}"))
        })
    }

    /// Accept a pending chat-request / group invite. After this returns the
    /// group is a normal active chat.
    pub fn accept_group_invite(&self, group_hex: &str) -> Result<AppGroupRecord> {
        let group_id = group_id_from_hex(group_hex)?;
        let label = self.active_label();
        let runtime = self.runtime.clone();
        self.tokio.block_on(async move {
            runtime
                .accept_group_invite(&label, &group_id)
                .await
                .map_err(|e| anyhow!("accept_group_invite: {e}"))
        })
    }

    /// Decline a pending chat-request / group invite. Used for "Block".
    pub fn decline_group_invite(&self, group_hex: &str) -> Result<()> {
        let group_id = group_id_from_hex(group_hex)?;
        let label = self.active_label();
        let runtime = self.runtime.clone();
        self.tokio.block_on(async move {
            runtime
                .decline_group_invite(&label, &group_id)
                .await
                .map(|_| ())
                .map_err(|e| anyhow!("decline_group_invite: {e}"))
        })
    }

    /// Toggle the archived flag on a group. Local-only — no relay traffic.
    pub fn set_group_archived(&self, group_hex: &str, archived: bool) -> Result<AppGroupRecord> {
        self.app
            .set_group_archived(&self.active_label(), group_hex, archived)
            .map_err(|e| anyhow!("set_group_archived: {e}"))
    }

    /// MLS roster for a group (account ids + any locally-known profile labels).
    /// Cached member list for a group. Served from the in-process cache —
    /// NEVER synchronously from the account worker (see `members_cache`).
    /// A cold entry returns empty and queues a background refresh; the cache
    /// is warmed for all groups at boot and re-refreshed on group events, so
    /// misses are rare.
    pub fn group_members(&self, group_hex: &str) -> Result<Vec<AppGroupMemberRecord>> {
        if let Some(m) = self.members_cache.lock().unwrap().get(group_hex) {
            return Ok(m.clone());
        }
        self.refresh_members_async(group_hex);
        Ok(Vec::new())
    }

    /// Member count from the cached member list (0 while the cache is cold).
    pub fn group_member_count(&self, group_hex: &str) -> usize {
        self.group_members(group_hex).map(|m| m.len()).unwrap_or(0)
    }

    /// Queue a background refresh of one group's member list into the cache.
    /// Deduped: a group with a refresh already in flight is skipped. The
    /// worker query is a fast local MLS read, but it can queue behind a
    /// long-running catch-up — hence always async, never on the UI thread.
    pub fn refresh_members_async(&self, group_hex: &str) {
        let Ok(group_id) = group_id_from_hex(group_hex) else {
            return;
        };
        {
            let mut inflight = self.members_inflight.lock().unwrap();
            if !inflight.insert(group_hex.to_string()) {
                return;
            }
        }
        let label = self.active_label();
        let runtime = self.runtime.clone();
        let cache = self.members_cache.clone();
        let inflight = self.members_inflight.clone();
        let key = group_hex.to_string();
        self.tokio.spawn(async move {
            let result = runtime.group_members(&label, &group_id).await;
            inflight.lock().unwrap().remove(&key);
            match result {
                Ok(members) => {
                    cache.lock().unwrap().insert(key, members);
                }
                Err(e) => eprintln!("[backend] members refresh ({key}) failed: {e}"),
            }
        });
    }

    /// Synchronously fill the members cache for every known group (visible +
    /// archived). Called from boot — which runs on a worker thread, never the
    /// UI thread — while the account worker is still idle, before the
    /// background network phase can occupy it. This way the very first
    /// chat-list build names 1:1 chats correctly instead of upgrading them a
    /// couple of seconds later.
    fn warm_members_cache(&self) {
        let t = std::time::Instant::now();
        let groups = self.app.groups(&self.active_label()).unwrap_or_default();
        for g in &groups {
            let Ok(group_id) = group_id_from_hex(&g.group_id_hex) else {
                continue;
            };
            let label = self.active_label();
            let rt = self.runtime.clone();
            match self
                .tokio
                .block_on(async move { rt.group_members(&label, &group_id).await })
            {
                Ok(members) => {
                    self.members_cache
                        .lock()
                        .unwrap()
                        .insert(g.group_id_hex.clone(), members);
                }
                Err(e) => {
                    eprintln!("[backend] warm members ({}) failed: {e}", g.group_id_hex)
                }
            }
        }
        eprintln!(
            "[boot-timing] members cache warmed for {} groups in {:?}",
            groups.len(),
            t.elapsed()
        );
    }

    pub fn group_mls_state(&self, group_hex: &str) -> Result<AppGroupMlsState> {
        let group_id = group_id_from_hex(group_hex)?;
        let label = self.active_label();
        let runtime = self.runtime.clone();
        self.tokio.block_on(async move {
            runtime
                .group_mls_state(&label, &group_id)
                .await
                .map_err(|e| anyhow!("group_mls_state: {e}"))
        })
    }

    /// Best-effort profile picture URL for an account id. Cache-backed —
    /// see [`Backend::account_name_and_picture`].
    pub fn account_picture_url(&self, account_id_hex: &str) -> Option<String> {
        self.account_name_and_picture(account_id_hex).1
    }

    /// Best-effort display name for an account id (cache, then hex tail).
    pub fn account_display_name(&self, account_id_hex: &str) -> String {
        self.account_name_and_picture(account_id_hex).0
    }

    /// Display name + picture URL for an account id, served from the
    /// in-process profile cache — NEVER synchronously from the directory
    /// storage (see `profile_cache`). A cold entry returns the hex-tail
    /// fallback and queues a background refresh; the cache is warmed at boot
    /// for every group member + contact, so misses are rare.
    pub fn account_name_and_picture(&self, account_id_hex: &str) -> (String, Option<String>) {
        let key = account_id_hex.to_ascii_lowercase();
        if let Some(v) = self.profile_cache.lock().unwrap().get(&key) {
            return v.clone();
        }
        self.refresh_profile_cache_async(account_id_hex);
        let fallback = if account_id_hex.eq_ignore_ascii_case(&self.active_id()) {
            "You".to_string()
        } else {
            short_account_id(account_id_hex)
        };
        (fallback, None)
    }

    /// The uncached directory read backing the profile cache. Reads marmot's
    /// shared directory storage — can block behind the background sync's
    /// writes, so only the boot warm-up and async refreshes call it.
    fn name_and_picture_direct(
        app: &MarmotApp,
        my_account_id_hex: &str,
        account_id_hex: &str,
    ) -> (String, Option<String>) {
        let is_self = account_id_hex.eq_ignore_ascii_case(my_account_id_hex);
        let entry = app
            .directory_entry_for_account_id(account_id_hex)
            .ok()
            .flatten();
        let profile = entry.and_then(|e| e.profile);
        let name = profile.as_ref().and_then(|p| {
            p.display_name
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| p.name.clone().filter(|s| !s.is_empty()))
        });
        let pic = profile
            .as_ref()
            .and_then(|p| p.picture.clone().filter(|s| !s.is_empty()));
        let name = name.unwrap_or_else(|| {
            if is_self {
                "You".to_string()
            } else {
                short_account_id(account_id_hex)
            }
        });
        (name, pic)
    }

    /// Queue a background refresh of one account's cached name/picture.
    /// Deduped per account id.
    pub fn refresh_profile_cache_async(&self, account_id_hex: &str) {
        let key = account_id_hex.to_ascii_lowercase();
        {
            let mut inflight = self.profile_inflight.lock().unwrap();
            if !inflight.insert(key.clone()) {
                return;
            }
        }
        let app = self.app.clone();
        let me = self.active_id();
        let cache = self.profile_cache.clone();
        let inflight = self.profile_inflight.clone();
        self.tokio.spawn(async move {
            let v = Self::name_and_picture_direct(&app, &me, &key);
            inflight.lock().unwrap().remove(&key);
            cache.lock().unwrap().insert(key, v);
        });
    }

    /// Queue background refreshes for every cached profile. Called when the
    /// background directory sync completes, so names/pictures that changed
    /// while we were offline converge shortly after.
    pub fn refresh_all_profiles_async(&self) {
        let keys: Vec<String> = self.profile_cache.lock().unwrap().keys().cloned().collect();
        for k in keys {
            self.refresh_profile_cache_async(&k);
        }
    }

    /// Synchronously fill the profile cache for self, every known group
    /// member, and every contact. Called from boot (worker thread) before
    /// the background sync starts writing to the directory storage.
    fn warm_profile_cache(&self) {
        let t = std::time::Instant::now();
        let mut ids: HashSet<String> = HashSet::new();
        ids.insert(self.active_id().to_ascii_lowercase());
        for members in self.members_cache.lock().unwrap().values() {
            for m in members {
                ids.insert(m.member_id_hex.to_ascii_lowercase());
            }
        }
        if let Ok(follows) = self.follow_list() {
            for f in &follows {
                ids.insert(f.account_id_hex.to_ascii_lowercase());
            }
        }
        let count = ids.len();
        for id in ids {
            let v = Self::name_and_picture_direct(&self.app, &self.active_id(), &id);
            self.profile_cache.lock().unwrap().insert(id, v);
        }
        eprintln!(
            "[boot-timing] profile cache warmed for {count} accounts in {:?}",
            t.elapsed()
        );
    }

    /// Full profile metadata for an account id from the local directory cache
    /// (own profile reads from disk). No network — `None` simply means the
    /// account isn't cached yet; callers fall back to
    /// [`Backend::fetch_profile_async`].
    pub fn cached_profile(&self, account_id_hex: &str) -> Option<UserProfileMetadata> {
        if account_id_hex.eq_ignore_ascii_case(&self.active_id()) {
            return self.load_profile().ok().flatten();
        }
        self.app
            .directory_entry_for_account_id(account_id_hex)
            .ok()
            .flatten()
            .and_then(|entry| entry.profile)
    }

    /// Fetch an arbitrary account's kind-0 profile from the relays (configured
    /// set + the whitenoise discovery relays), warming the directory cache,
    /// then hand the result to `on_done` on the tokio runtime. Used by the
    /// profile modal for @-mentioned users who aren't in any shared group and
    /// therefore have no cached directory entry.
    pub fn fetch_profile_async<F>(&self, account_id_hex: &str, on_done: F)
    where
        F: FnOnce(Option<UserProfileMetadata>) + Send + 'static,
    {
        let app = self.app.clone();
        let relays = self.discovery_relays();
        let id = account_id_hex.to_string();
        self.tokio.spawn(async move {
            if let Err(e) = app.refresh_profile_for_account_id(&id, relays).await {
                tracing::warn!(target: "backend::profile", account = %id, error = %e, "relay profile fetch failed");
            }
            let profile = app
                .directory_entry_for_account_id(&id)
                .ok()
                .flatten()
                .and_then(|entry| entry.profile);
            on_done(profile);
        });
    }

    /// For a 1:1 chat (exactly two members) return the *other* member's account
    /// id hex. Returns `None` for self-only or multi-party groups, so callers
    /// can fall back to the MLS group profile name for real group chats.
    pub fn direct_chat_peer(&self, group_hex: &str) -> Option<String> {
        let members = self.group_members(group_hex).ok()?;
        if members.len() != 2 {
            return None;
        }
        let me = &self.active_id();
        members
            .into_iter()
            .map(|m| m.member_id_hex)
            .find(|id| !id.eq_ignore_ascii_case(me))
    }

    /// Ensure the active account has published its NIP-65 (and inbox) relay
    /// lists. `marmot-app` fails group creation closed with `missing account
    /// relay lists: ["nip65"]` when the account has never published a kind-10002
    /// list. That happens whenever the account first booted with no relays, or
    /// the login-time publish failed: relay lists are only published in
    /// `login_account`, which is skipped once the account is already present in
    /// the home, so a stale account can never recover them on its own.
    ///
    /// Idempotent — returns early when the cached status is already complete,
    /// and publishes only the kinds reported missing. A no-op when no relays are
    /// configured, so the downstream op still surfaces the real error instead of
    /// us papering over a genuinely unconfigured account.
    fn ensure_account_relay_lists(&self) -> Result<()> {
        if self.relays.is_empty() {
            return Ok(());
        }
        let status = self
            .app
            .account_relay_list_status_for_account_id(&self.active_id())
            .map_err(|e| anyhow!("account_relay_list_status: {e}"))?;
        if status.complete {
            return Ok(());
        }
        let endpoints: Vec<TransportEndpoint> = self
            .relays
            .iter()
            .cloned()
            .map(TransportEndpoint::from)
            .collect();
        let label = self.active_label();
        let runtime = self.runtime.clone();
        let missing = status.missing.clone();
        self.tokio.block_on(async move {
            for kind in &missing {
                let token = kind.token();
                runtime
                    .publish_account_relay_list_kind(
                        &label,
                        token,
                        endpoints.clone(),
                        endpoints.clone(),
                    )
                    .await
                    .map_err(|e| anyhow!("publish_account_relay_list_kind({token}): {e}"))?;
            }
            Ok::<_, anyhow::Error>(())
        })
    }

    /// Republish the account's NIP-65 and inbox relay lists, declaring *all*
    /// currently-configured relays and publishing the events to that full set.
    ///
    /// This is the "Republish relay list" button. It exists because the account
    /// may have a stale list that names only a subset of relays (e.g. only the
    /// first relay that acked at first login), which makes the account
    /// undiscoverable to peers who only query the relays it omits. Forcing a
    /// republish to the full configured set fixes that. Returns the number of
    /// relays declared.
    pub fn republish_relay_lists(&self) -> Result<usize> {
        if self.relays.is_empty() {
            return Err(anyhow!("No relays configured."));
        }
        let endpoints: Vec<TransportEndpoint> = self
            .relays
            .iter()
            .cloned()
            .map(TransportEndpoint::from)
            .collect();
        let label = self.active_label();
        let runtime = self.runtime.clone();
        let count = self.relays.len();
        self.tokio.block_on(async move {
            for kind in ["nip65", "inbox"] {
                runtime
                    .publish_account_relay_list_kind(
                        &label,
                        kind,
                        endpoints.clone(),
                        endpoints.clone(),
                    )
                    .await
                    .map_err(|e| anyhow!("publish_account_relay_list_kind({kind}): {e}"))?;
            }
            Ok::<_, anyhow::Error>(())
        })?;
        Ok(count)
    }

    /// The relay set we use to *discover* peers: the user's configured relays
    /// plus the well-known public indexers, deduped.
    fn discovery_relays(&self) -> Vec<TransportEndpoint> {
        let mut urls: Vec<String> = self.relays.clone();
        for r in DISCOVERY_RELAYS {
            if !urls.iter().any(|u| u.eq_ignore_ascii_case(r)) {
                urls.push((*r).to_string());
            }
        }
        urls.into_iter().map(TransportEndpoint::from).collect()
    }

    /// Resolve every invited member's relay list + key package against the broad
    /// discovery set *before* group creation, warming the directory cache so the
    /// runtime's own `member_key_package` lookup (which only sees the configured
    /// relays) succeeds. Returns the list of members we could not resolve, with
    /// the reason — so the caller can surface a clear, peer-named error instead
    /// of the cryptic `missing account relay lists: ["nip65"]`, which actually
    /// refers to the *peer's* missing list, not the local account's.
    fn prewarm_members(&self, members: &[String]) -> Vec<(String, String)> {
        let broad = self.discovery_relays();
        let mut unresolved = Vec::new();
        for member in members {
            // Local accounts resolve from disk; no relay lookup needed.
            if self.account_home.account(member).is_ok() {
                tracing::info!(target: "backend::prewarm", member = %member, "member is a local account");
                continue;
            }
            // marmot's `fetch_latest_key_package_for_account_id` does its relay
            // *queries* by parsing the arg to hex internally, but filters the
            // returned KeyPackage records by comparing `event.pubkey` (hex)
            // against the raw arg string. Passing an npub there makes the relay
            // list resolve (it re-derives hex) yet every KP record gets filtered
            // out (hex != npub) → a bogus `MissingKeyPackage`. So normalize to
            // hex ourselves before calling in.
            let account_id_hex = match nostr::PublicKey::parse(member) {
                Ok(pk) => pk.to_hex(),
                Err(_) => {
                    unresolved.push((member.clone(), "not a valid npub or hex pubkey".to_string()));
                    continue;
                }
            };
            let app = self.app.clone();
            let broad = broad.clone();
            let result = self.tokio.block_on(async move {
                app.fetch_latest_key_package_for_account_id(&account_id_hex, broad)
                    .await
            });
            match result {
                Ok(_) => {
                    tracing::info!(target: "backend::prewarm", member = %member, "resolved key package via discovery relays");
                }
                Err(e) => {
                    tracing::warn!(target: "backend::prewarm", member = %member, error = %e, "could not resolve member");
                    unresolved.push((member.clone(), e.to_string()));
                }
            }
        }
        unresolved
    }
}

// Explicit path so this resolves to `src/backend/groups.rs` both for the normal
// build and for the test-harness binaries that `#[path = "../backend.rs"]`-include
// this file from `src/bin/` (where a bare `mod groups;` would look in `src/`).
#[path = "backend/groups.rs"]
mod groups;
pub use groups::*;
