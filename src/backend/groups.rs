use super::*;

impl Backend {
    /// Create a 1:1 or group chat with the listed members (npub or hex pubkey).
    pub fn create_group(&self, name: &str, members: &[String]) -> Result<GroupId> {
        // The account must have a published NIP-65 list before the runtime will
        // create a group; backfill it if a prior boot left it missing.
        self.ensure_account_relay_lists()?;

        // Diagnostic: log our own relay-list status so a "missing nip65" failure
        // can be unambiguously attributed to us vs. a peer.
        match self
            .app
            .account_relay_list_status_for_account_id(&self.active_id())
        {
            Ok(status) => tracing::info!(
                target: "backend::create_group",
                complete = status.complete,
                missing = ?status.missing,
                nip65 = ?status.nip65.relays,
                "local account relay-list status before create_group"
            ),
            Err(e) => {
                tracing::warn!(target: "backend::create_group", error = %e, "could not read local relay-list status")
            }
        }

        // Warm the directory cache for each peer against the broad discovery set
        // and fail early with a clear, peer-named error if any can't be found.
        let unresolved = self.prewarm_members(members);
        if !unresolved.is_empty() {
            let detail = unresolved
                .iter()
                .map(|(m, e)| format!("{m} ({e})"))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(anyhow!(
                "can't reach these contacts — they haven't published a relay list / key package to any relay we know: {detail}"
            ));
        }

        let label = self.active_label();
        let members = members.to_vec();
        let runtime = self.runtime.clone();
        let name = name.to_string();
        self.tokio.block_on(async move {
            runtime
                .create_group(&label, &name, &members, None)
                .await
                .map_err(|e| anyhow!("create_group: {e}"))
        })
    }

    /// Invite additional members into an existing group. Caller must be an
    /// admin of the group (the runtime enforces this; non-admins get an error).
    /// `members` are npub or hex pubkey strings; the runtime fetches each
    /// peer's key package off the relay set before committing.
    pub fn invite_members(&self, group_hex: &str, members: &[String]) -> Result<SendSummary> {
        let group_id = group_id_from_hex(group_hex)?;

        // Same preflight as create_group: the runtime resolves each invitee's
        // relay list + key package against only the configured relays, so a
        // peer who published solely to the discovery indexers fails with the
        // cryptic `missing account relay lists: ["nip65"]` (the *peer's* list,
        // not ours). Warm the cache against the broad set and name the peer.
        self.ensure_account_relay_lists()?;
        let unresolved = self.prewarm_members(members);
        if !unresolved.is_empty() {
            let detail = unresolved
                .iter()
                .map(|(m, e)| format!("{m} ({e})"))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(anyhow!(
                "can't reach these contacts — they haven't published a relay list / key package to any relay we know: {detail}"
            ));
        }

        let label = self.active_label();
        let members = members.to_vec();
        let runtime = self.runtime.clone();
        self.tokio.block_on(async move {
            runtime
                .invite_members(&label, &group_id, &members)
                .await
                .map_err(|e| anyhow!("invite_members: {e}"))
        })
    }

    /// Promote a group member to admin. Caller must already be an admin (the
    /// engine enforces this on the outbound MLS commit; non-admins get an
    /// error). `member_ref` is an npub, hex pubkey, or known account label —
    /// `member_id_hex` from a group-member record works directly.
    pub fn promote_admin(&self, group_hex: &str, member_ref: &str) -> Result<SendSummary> {
        let group_id = group_id_from_hex(group_hex)?;
        let label = self.active_label();
        let member_ref = member_ref.to_string();
        let runtime = self.runtime.clone();
        self.tokio.block_on(async move {
            runtime
                .promote_admin(&label, &group_id, &member_ref)
                .await
                .map_err(|e| anyhow!("promote_admin: {e}"))
        })
    }

    /// Demote a group admin back to a regular member. Caller must be an admin
    /// (the engine enforces this; non-admins get an error). `member_ref` is an
    /// npub, hex pubkey, or known account label.
    pub fn demote_admin(&self, group_hex: &str, member_ref: &str) -> Result<SendSummary> {
        let group_id = group_id_from_hex(group_hex)?;
        let label = self.active_label();
        let member_ref = member_ref.to_string();
        let runtime = self.runtime.clone();
        self.tokio.block_on(async move {
            runtime
                .demote_admin(&label, &group_id, &member_ref)
                .await
                .map_err(|e| anyhow!("demote_admin: {e}"))
        })
    }

    /// Relinquish the active account's own admin rights on `group_hex`.
    pub fn self_demote_admin(&self, group_hex: &str) -> Result<SendSummary> {
        let group_id = group_id_from_hex(group_hex)?;
        let label = self.active_label();
        let runtime = self.runtime.clone();
        self.tokio.block_on(async move {
            runtime
                .self_demote_admin(&label, &group_id)
                .await
                .map_err(|e| anyhow!("self_demote_admin: {e}"))
        })
    }

    /// Rename a group. Caller must be an admin (the engine enforces this on the
    /// outbound MLS commit; non-admins get an error). Publishes the new name via
    /// the group's `marmot.group.profile.v1` component, leaving the description
    /// untouched.
    pub fn rename_group(&self, group_hex: &str, new_name: &str) -> Result<SendSummary> {
        let group_id = group_id_from_hex(group_hex)?;
        let label = self.active_label();
        let name = new_name.to_string();
        let runtime = self.runtime.clone();
        self.tokio.block_on(async move {
            runtime
                .update_group_profile(&label, &group_id, Some(name), None)
                .await
                .map_err(|e| anyhow!("rename_group: {e}"))
        })
    }

    /// Encrypt + upload a new group avatar to Blossom and publish the group image
    /// component (admin-only, enforced by the engine). Non-blocking: runs on the
    /// tokio runtime and fires `on_done` on a worker thread. Passing empty `bytes`
    /// clears the image.
    pub fn set_group_image_async<F>(
        &self,
        group_hex: &str,
        bytes: Vec<u8>,
        media_type: String,
        on_done: F,
    ) where
        F: FnOnce(Result<SendSummary>) + Send + 'static,
    {
        let group_id = match group_id_from_hex(group_hex) {
            Ok(id) => id,
            Err(e) => {
                on_done(Err(e));
                return;
            }
        };
        let label = self.active_label();
        let runtime = self.runtime.clone();
        self.tokio.spawn(async move {
            let result = runtime
                .update_group_image(&label, &group_id, bytes, media_type)
                .await
                .map_err(|e| anyhow!("set_group_image: {e}"));
            on_done(result);
        });
    }

    /// Fetch + decrypt the group's avatar into raw image bytes (PNG/JPEG/etc.).
    /// Non-blocking; `on_done` fires on a worker thread. Errors when the group
    /// has no image set.
    pub fn fetch_group_image_async<F>(&self, group_hex: &str, on_done: F)
    where
        F: FnOnce(Result<Vec<u8>>) + Send + 'static,
    {
        let group_id = match group_id_from_hex(group_hex) {
            Ok(id) => id,
            Err(e) => {
                on_done(Err(e));
                return;
            }
        };
        let label = self.active_label();
        let runtime = self.runtime.clone();
        self.tokio.spawn(async move {
            let result = runtime
                .download_group_image(&label, &group_id)
                .await
                .map_err(|e| anyhow!("fetch_group_image: {e}"));
            on_done(result);
        });
    }

    /// The group's current image content hash (hex), or `None` when no image is
    /// set. Doubles as a cache key and presence check for the avatar pipeline.
    pub fn group_image_hash(&self, group_hex: &str) -> Option<String> {
        let chats = self.chats().ok()?;
        let group = chats
            .iter()
            .find(|g| g.group_id_hex.eq_ignore_ascii_case(group_hex))?;
        if group.image.present && !group.image.image_hash_hex.is_empty() {
            Some(group.image.image_hash_hex.clone())
        } else {
            None
        }
    }

    /// The group's admin set as 32-byte hex pubkeys (same encoding as
    /// `account_id_hex`). Empty when the group is unknown.
    pub fn group_admins(&self, group_hex: &str) -> Vec<String> {
        let Ok(chats) = self.chats() else {
            return Vec::new();
        };
        chats
            .iter()
            .find(|g| g.group_id_hex.eq_ignore_ascii_case(group_hex))
            .map(|g| g.admin_policy.admins.clone())
            .unwrap_or_default()
    }

    /// True when the active account is an admin of `group_hex`. Looks at the
    /// group's admin policy component; the admins list contains 32-byte hex
    /// pubkeys, identical encoding to `account_id_hex`.
    pub fn is_group_admin(&self, group_hex: &str) -> bool {
        let me = &self.active_id();
        let Ok(chats) = self.chats() else {
            return false;
        };
        let Some(group) = chats
            .iter()
            .find(|g| g.group_id_hex.eq_ignore_ascii_case(group_hex))
        else {
            return false;
        };
        group
            .admin_policy
            .admins
            .iter()
            .any(|a| a.eq_ignore_ascii_case(me))
    }

    /// Local key-package records for the active account. Sync — reads the
    /// on-disk JSON next to the account home. Use `key_packages_fetch()` for
    /// the network-augmented view (local + what's actually on the relay).
    pub fn key_packages_local(&self) -> Vec<marmot_app::AccountKeyPackageRecord> {
        self.app
            .local_key_package_records(&self.active_label())
            .unwrap_or_default()
    }

    /// Full key-package state: local + a relay snapshot from the account's
    /// configured key-package relays. Bootstrap relay list is whatever the
    /// account was booted with — empty means use the cached relay list.
    pub fn key_packages_fetch(&self) -> Result<Vec<marmot_app::AccountKeyPackageRecord>> {
        let label = self.active_label();
        let app = self.app.clone();
        let bootstrap: Vec<TransportEndpoint> = self
            .relays
            .iter()
            .cloned()
            .map(TransportEndpoint::from)
            .collect();
        self.tokio.block_on(async move {
            app.account_key_package_records(&label, bootstrap)
                .await
                .map_err(|e| anyhow!("account_key_package_records: {e}"))
        })
    }

    /// Relays this account uses for key-package publishing. After the upstream
    /// relay-list rework there is no dedicated kind-10051 list — KeyPackages
    /// publish to the account's NIP-65 (kind 10002) outbox relays, falling back
    /// to the configured bootstrap relays when no NIP-65 list exists yet.
    pub fn key_package_relays(&self) -> Vec<String> {
        let nip65 = self
            .app
            .account_relay_list_status_for_account_id(&self.active_id())
            .map(|status| status.nip65.relays)
            .unwrap_or_default();
        if nip65.is_empty() {
            self.relays.clone()
        } else {
            nip65
        }
    }

    /// Publish a fresh key package for the active account. Returns the number
    /// of relays that acked the publish. Same call as the runtime worker's
    /// `PublishKeyPackage` command.
    pub fn publish_key_package(&self) -> Result<usize> {
        let label = self.active_label();
        let runtime = self.runtime.clone();
        self.tokio.block_on(async move {
            runtime
                .publish_key_package(&label)
                .await
                .map_err(|e| anyhow!("publish_key_package: {e}"))
        })
    }

    /// Rotate the key package: invalidate the current one (delete-event on
    /// the relay set) and publish a fresh one. Returns the relay-ack count
    /// for the new publish.
    pub fn rotate_key_package(&self) -> Result<usize> {
        let label = self.active_label();
        let runtime = self.runtime.clone();
        self.tokio.block_on(async move {
            runtime
                .rotate_key_package(&label)
                .await
                .map_err(|e| anyhow!("rotate_key_package: {e}"))
        })
    }

    /// Fetch a *contact's* latest published key package from their relays
    /// (broad discovery set), returning the event's created-at (unix secs) and
    /// the relays it was found on — the real freshness data the contact-detail
    /// IDENTITY panel shows. Sync + blocking: call from a worker thread, never
    /// the UI thread. Accepts npub or hex (normalized internally by marmot).
    pub fn fetch_contact_key_package(&self, account_id: &str) -> Result<(u64, Vec<String>)> {
        let account_id_hex = nostr::PublicKey::parse(account_id)
            .map(|pk| pk.to_hex())
            .map_err(|_| anyhow!("not a valid npub or hex pubkey"))?;
        let broad = self.discovery_relays();
        let app = self.app.clone();
        let fetched = self
            .tokio
            .block_on(async move {
                app.fetch_latest_key_package_for_account_id(&account_id_hex, broad)
                    .await
            })
            .map_err(|e| anyhow!("fetch_latest_key_package_for_account_id: {e}"))?;
        Ok((fetched.created_at, fetched.source_relays))
    }

    /// True when there is at least one locally-recorded key package for the
    /// active account. Used at boot to decide whether to bootstrap-publish.
    pub fn has_local_key_package(&self) -> bool {
        !self.key_packages_local().is_empty()
    }

    /// Relay URLs the running backend was booted with. The on-disk list
    /// (via `load_relays()`) may differ if the user edited it since launch —
    /// in that case the UI shows a "restart to apply" banner.
    pub fn booted_relays(&self) -> &[String] {
        &self.relays
    }

    /// Aggregate connection health of the relay plane. Returns
    /// `(connected, total)`. `total` counts only relays the SDK is currently
    /// tracking, which can lag the configured list briefly after boot.
    pub fn relay_health(&self) -> (usize, usize) {
        let plane = self.runtime.shared_services().relay_plane().clone();
        let health = self
            .tokio
            .block_on(async move { plane.relay_health().await });
        (health.connected, health.total_relays)
    }

    // Diagnostic JSON snapshot — account info, key packages on disk, and
    // projected group rows (visible + archived). Used by the in-app debug
    // pane to surface state that's otherwise locked behind encrypted SQLite.
    // ─── Security & privacy / developer settings ───────────────────────
    // These mirror the darkmatter-android "Security & Privacy" + "Developer"
    // settings cluster. Telemetry + audit-log *enabled* state live in marmot's
    // shared storage (read/written via MarmotApp), not our local settings JSON.

    /// Whether anonymous relay-connection telemetry export is enabled.
    pub fn telemetry_enabled(&self) -> bool {
        self.app
            .relay_telemetry_settings()
            .map(|s| s.export_enabled)
            .unwrap_or(false)
    }

    /// Toggle anonymous relay telemetry export, preserving the export interval.
    pub fn set_telemetry_enabled(&self, on: bool) -> Result<()> {
        let current = self
            .app
            .relay_telemetry_settings()
            .map_err(|e| anyhow!("relay_telemetry_settings: {e}"))?;
        self.app
            .set_relay_telemetry_settings(RelayTelemetrySettings {
                export_enabled: on,
                export_interval_seconds: current.export_interval_seconds,
            })
            .map_err(|e| anyhow!("set_relay_telemetry_settings: {e}"))?;
        Ok(())
    }

    /// Whether the per-account forensic audit log (JSONL) is enabled.
    pub fn audit_logs_enabled(&self) -> bool {
        self.app
            .audit_log_settings()
            .map(|s| s.enabled)
            .unwrap_or(false)
    }

    /// Enable/disable forensic audit logging. Persists the switch and applies
    /// it to running sessions in place via marmot's recorder hot-swap, so no
    /// restart is needed. The returned future must run on the backend tokio
    /// runtime — applying the switch awaits each account worker's FIFO queue,
    /// which a misbehaving relay can hold for its full connection timeout.
    pub fn set_audit_logs_enabled(
        &self,
        on: bool,
    ) -> impl std::future::Future<Output = Result<()>> + Send + 'static {
        let runtime = self.runtime.clone();
        async move {
            runtime
                .set_audit_log_settings(AuditLogSettings {
                    enabled: on,
                    ..AuditLogSettings::default()
                })
                .await
                .map_err(|e| anyhow!("set_audit_log_settings: {e}"))?;
            Ok(())
        }
    }

    /// On-disk forensic audit-log files (JSONL) across all accounts. Reads
    /// the account directories directly; cheap, but still disk IO — call off
    /// the UI thread.
    pub fn audit_log_files(&self) -> Result<Vec<AuditLogFile>> {
        self.runtime
            .audit_log_files()
            .map_err(|e| anyhow!("audit_log_files: {e}"))
    }

    /// Delete one audit-log file by path. Resolves to `still_recording`:
    /// `true` means a live recorder owned the file and rotated to a fresh one
    /// (recording continues), `false` means the file was simply removed.
    pub fn delete_audit_log_file(
        &self,
        path: String,
    ) -> impl std::future::Future<Output = Result<bool>> + Send + 'static {
        let runtime = self.runtime.clone();
        async move {
            let outcome = runtime
                .delete_audit_log_file(&path)
                .await
                .map_err(|e| anyhow!("delete_audit_log_file: {e}"))?;
            Ok(outcome.still_recording)
        }
    }

    pub fn debug_snapshot(&self) -> String {
        use serde_json::{Value, json};
        let chats = self.chats().unwrap_or_default();
        let archived = self.archived_chats().unwrap_or_default();
        let key_packages = read_key_packages_dir(&self.home);
        let active = self.account();
        let account = json!({
            "label": active.label,
            "account_id_hex": active.account_id_hex,
            "npub": marmot_app::npub_for_account_id(&active.account_id_hex).ok(),
        });
        let group_to_json = |g: &AppGroupRecord| -> Value {
            // MLS internals — the developer/diagnostics view (mirrors the
            // darkmatter-android "MLS" card): live epoch, member count, and
            // required app components from the running engine.
            let mls = match self.group_mls_state(&g.group_id_hex) {
                Ok(s) => json!({
                    "epoch": s.epoch,
                    "member_count": s.member_count,
                    "required_app_components": s.required_app_components,
                }),
                Err(e) => json!({ "error": e.to_string() }),
            };
            json!({
                "group_id_hex": g.group_id_hex,
                "name": g.profile.name,
                "description": g.profile.description,
                "archived": g.archived,
                "pending_confirmation": g.pending_confirmation,
                "welcomer_account_id_hex": g.welcomer_account_id_hex,
                "via_welcome_message_id_hex": g.via_welcome_message_id_hex,
                "nostr_group_id_hex": g.nostr_routing.nostr_group_id_hex,
                "relays": g.nostr_routing.relays,
                "admins": g.admin_policy.admins,
                "mls": mls,
            })
        };
        let dump = json!({
            "home": self.home.display().to_string(),
            "relays": self.relays,
            "account": account,
            "key_packages": key_packages,
            "groups_visible": chats.iter().map(group_to_json).collect::<Vec<_>>(),
            "groups_archived": archived.iter().map(group_to_json).collect::<Vec<_>>(),
        });
        serde_json::to_string_pretty(&dump).unwrap_or_else(|e| format!("serialize error: {e}"))
    }

    /// Pump live chat-list updates for the active account onto the Slint
    /// event loop.
    ///
    /// `on_update` is invoked on a tokio worker; it should re-marshal onto the
    /// Slint main thread via `slint::invoke_from_event_loop`.
    ///
    /// Returns a `JoinHandle` so the caller can `.abort()` the watcher on
    /// account switch — the subscription is bound to the label it was created
    /// with, so a stale watcher would keep pushing the previous account's
    /// chats into the UI.
    pub fn watch_chats<F>(&self, mut on_update: F) -> JoinHandle<()>
    where
        F: FnMut(AppGroupRecord) + Send + 'static,
    {
        let label = self.active_label();
        let runtime = self.runtime.clone();
        self.tokio.spawn(async move {
            let mut sub = match runtime.subscribe_chats(&label, false) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[backend] subscribe_chats failed: {e}");
                    return;
                }
            };
            while let Some(update) = sub.recv().await {
                on_update(update);
            }
        })
    }

    /// Pump live message updates for a single group. The callback receives a
    /// `RuntimeMessageUpdate`; the caller decides how to project it.
    ///
    /// Returns a `JoinHandle` so the caller can `.abort()` the watcher when
    /// the user switches chats (otherwise watchers accumulate forever).
    pub fn watch_messages<F>(&self, group_hex: &str, mut on_update: F) -> JoinHandle<()>
    where
        F: FnMut(RuntimeMessageUpdate) + Send + 'static,
    {
        let label = self.active_label();
        let runtime = self.runtime.clone();
        // The subscription snapshot only seeds marmot's internal "already
        // seen" dedup set — we never read it. `limit: None` would decrypt the
        // group's ENTIRE history on every chat switch just for that set.
        // Keep it at 1: re-emitted old events slip through marmot's dedup,
        // but the UI handler is idempotent (find_message_row /
        // refresh_one_message_row), so duplicates are no-ops there.
        let query = AppMessageQuery {
            group_id_hex: Some(group_hex.to_string()),
            limit: Some(1),
        };
        self.tokio.spawn(async move {
            let mut sub: RuntimeMessagesSubscription =
                match runtime.subscribe_messages(&label, query).await {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[backend] subscribe_messages failed: {e}");
                        return;
                    }
                };
            while let Some(update) = sub.recv().await {
                on_update(update);
            }
        })
    }
}

/// Best-effort dump of the `key-packages/` directory next to the account home.
/// We surface filename + a few well-known fields; private material stays in the
/// blob and is never read out here.
pub(crate) fn read_key_packages_dir(home: &Path) -> Vec<serde_json::Value> {
    use serde_json::{Value, json};
    let dir = home.join("key-packages");
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let val: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => continue,
        };
        out.push(json!({
            "file": path.file_name().and_then(|s| s.to_str()).unwrap_or(""),
            "account_label": val.get("account_label"),
            "account_id_hex": val.get("account_id_hex"),
            "key_package_id": val.get("key_package_id"),
            "key_package_ref_hex": val.get("key_package_ref_hex"),
            "key_package_event_id": val.get("key_package_event_id"),
            "published_at": val.get("published_at"),
        }));
    }
    out
}

pub fn default_home() -> PathBuf {
    if let Some(p) = std::env::var_os("DM_HOME") {
        return PathBuf::from(p);
    }
    directories::ProjectDirs::from("", "", "darkmatter")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("./dm-home"))
}

pub(crate) fn group_id_from_hex(group_hex: &str) -> Result<GroupId> {
    let bytes = hex::decode(group_hex).context("decode group id")?;
    Ok(GroupId::new(bytes))
}

/// Whether an `upload_media` error means the group's encrypted-media policy
/// component is unusable (stale pre-#319 encoding that no longer decodes, or
/// absent/disabled) rather than a transient failure. These are the cases a
/// re-publish of the policy via `replace_encrypted_media_blob_endpoints` can
/// fix; anything else (network, encryption, send) must not trigger a heal.
pub(crate) fn is_stale_encrypted_media_policy(msg: &str) -> bool {
    msg.contains("encrypted media format must be")
        || msg.contains("encrypted media policy has no default endpoint")
        || msg.contains("group does not require encrypted media")
}

const MEDIA_REDIRECT_LIMIT: usize = 5;
const MEDIA_REDIRECT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MEDIA_REDIRECT_READ_TIMEOUT: Duration = Duration::from_secs(15);
const MEDIA_REDIRECT_TOTAL_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) async fn download_media_with_redirect_retry(
    runtime: MarmotAppRuntime,
    label: String,
    group_id: GroupId,
    reference: MediaAttachmentReference,
) -> Result<MediaDownloadResult> {
    match runtime
        .download_media(&label, &group_id, reference.clone())
        .await
    {
        Ok(download) => Ok(download),
        Err(err) => {
            let msg = err.to_string();
            if !is_media_redirect_error(&msg) {
                return Err(anyhow!("download_media: {err}"));
            }

            tracing::warn!(
                target: "backend::download_media",
                error = %msg,
                "encrypted-media download hit a Blossom redirect; resolving locator and retrying"
            );
            match resolve_media_reference_redirects(reference).await {
                Ok(Some(resolved_reference)) => runtime
                    .download_media(&label, &group_id, resolved_reference)
                    .await
                    .map_err(|retry| {
                        anyhow!("download_media (after redirect resolution): {retry}")
                    }),
                Ok(None) => Err(anyhow!("download_media: {err}")),
                Err(resolve_err) => Err(anyhow!(
                    "download_media: {err}; redirect resolution failed: {resolve_err:#}"
                )),
            }
        }
    }
}

pub(crate) fn is_media_redirect_error(msg: &str) -> bool {
    msg.contains("download returned HTTP")
        && ["HTTP 301", "HTTP 302", "HTTP 303", "HTTP 307", "HTTP 308"]
            .iter()
            .any(|status| msg.contains(status))
}

pub(crate) async fn resolve_media_reference_redirects(
    mut reference: MediaAttachmentReference,
) -> Result<Option<MediaAttachmentReference>> {
    let expected_hash = reference.ciphertext_sha256.to_ascii_lowercase();
    let mut changed = false;
    let mut last_error = None;
    for locator in &mut reference.locators {
        if locator.kind != BLOSSOM_LOCATOR_KIND_V1 {
            continue;
        }
        match resolve_media_locator_redirects(&locator.value, &expected_hash).await {
            Ok(resolved) => {
                if resolved != locator.value {
                    locator.value = resolved;
                    changed = true;
                }
            }
            Err(err) => {
                tracing::warn!(
                    target: "backend::download_media",
                    locator = %locator.value,
                    error = %err,
                    "could not resolve media locator redirect"
                );
                last_error = Some(err);
            }
        }
    }
    if changed {
        Ok(Some(reference))
    } else if let Some(err) = last_error {
        Err(err)
    } else {
        Ok(None)
    }
}

pub(crate) async fn resolve_media_locator_redirects(
    value: &str,
    expected_hash: &str,
) -> Result<String> {
    let mut current = reqwest::Url::parse(value).context("parse media locator URL")?;
    validate_media_fetch_url(&current).map_err(|err| anyhow!("unsafe Blossom URL: {err}"))?;

    for _ in 0..MEDIA_REDIRECT_LIMIT {
        let client = media_redirect_client_for_url(&current).await?;
        let response = client
            .get(current.clone())
            .send()
            .await
            .context("request media locator")?;
        if !response.status().is_redirection() {
            return Ok(current.to_string());
        }

        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .ok_or_else(|| anyhow!("redirect response did not include Location"))?
            .to_str()
            .context("redirect Location header is not UTF-8")?;
        let next = current
            .join(location)
            .context("redirect Location header is not a valid URL")?;
        validate_media_fetch_url(&next)
            .map_err(|err| anyhow!("unsafe Blossom redirect URL: {err}"))?;
        if !media_url_contains_hash(&next, expected_hash) {
            return Err(anyhow!(
                "redirect URL does not include the expected encrypted blob hash"
            ));
        }
        current = next;
    }

    Err(anyhow!(
        "media redirect chain exceeded {MEDIA_REDIRECT_LIMIT} hops"
    ))
}

pub(crate) async fn media_redirect_client_for_url(url: &reqwest::Url) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(MEDIA_REDIRECT_CONNECT_TIMEOUT)
        .read_timeout(MEDIA_REDIRECT_READ_TIMEOUT)
        .timeout(MEDIA_REDIRECT_TOTAL_TIMEOUT)
        .no_proxy();
    if let Some((domain, addrs)) = resolve_media_host(url).await? {
        builder = builder.resolve_to_addrs(&domain, &addrs);
    }
    builder.build().context("build media redirect HTTP client")
}

pub(crate) async fn resolve_media_host(
    url: &reqwest::Url,
) -> Result<Option<(String, Vec<SocketAddr>)>> {
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("Blossom URL is missing a host"))?;
    let allow_loopback = url.scheme() == "http" && cfg!(debug_assertions) && is_loopback_host(host);
    if let Ok(ip) = host.parse::<IpAddr>() {
        reject_non_public_ip(ip, allow_loopback)
            .map_err(|err| anyhow!("unsafe media host address: {err}"))?;
        return Ok(None);
    }

    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("Blossom URL is missing a fetch port"))?;
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .context("media host DNS lookup failed")?
        .collect::<Vec<_>>();
    if addrs.is_empty() {
        return Err(anyhow!("media host DNS lookup returned no addresses"));
    }
    for addr in &addrs {
        reject_non_public_ip(addr.ip(), allow_loopback)
            .map_err(|err| anyhow!("unsafe media host address: {err}"))?;
    }
    Ok(Some((host.to_ascii_lowercase(), addrs)))
}

pub(crate) fn validate_media_fetch_url(url: &reqwest::Url) -> std::result::Result<(), String> {
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URL must not include credentials".into());
    }
    if url.fragment().is_some() {
        return Err("URL must not include a fragment".into());
    }
    let host = url.host_str().ok_or("URL must include a host")?;
    match url.scheme() {
        "https" => validate_public_or_allowed_loopback_host(host, false),
        "http" if cfg!(debug_assertions) && is_loopback_host(host) => Ok(()),
        "http" => Err("URL scheme must be https".into()),
        _ => Err("URL scheme must be https".into()),
    }
}

pub(crate) fn validate_public_or_allowed_loopback_host(
    host: &str,
    allow_loopback: bool,
) -> std::result::Result<(), String> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return reject_non_public_ip(ip, allow_loopback);
    }
    if is_loopback_host(host) {
        if allow_loopback {
            Ok(())
        } else {
            Err("URL must not point at localhost".into())
        }
    } else {
        Ok(())
    }
}

pub(crate) fn is_loopback_host(host: &str) -> bool {
    let lowered = host.to_ascii_lowercase();
    lowered == "localhost"
        || lowered.ends_with(".localhost")
        || host.parse::<IpAddr>().is_ok_and(|addr| match addr {
            IpAddr::V4(addr) => addr.is_loopback(),
            IpAddr::V6(addr) => addr.is_loopback(),
        })
}

pub(crate) fn reject_non_public_ip(
    addr: IpAddr,
    allow_loopback: bool,
) -> std::result::Result<(), String> {
    match addr {
        IpAddr::V4(addr) if allow_loopback && addr.is_loopback() => Ok(()),
        IpAddr::V6(addr) if allow_loopback && addr.is_loopback() => Ok(()),
        IpAddr::V4(addr) if is_public_ipv4(addr) => Ok(()),
        IpAddr::V6(addr) if is_public_ipv6(addr) => Ok(()),
        _ => Err("URL must not point at a non-public address".into()),
    }
}

pub(crate) fn is_public_ipv4(addr: Ipv4Addr) -> bool {
    let [a, b, c, d] = addr.octets();
    !matches!(
        (a, b, c, d),
        (0, _, _, _)
            | (10, _, _, _)
            | (100, 64..=127, _, _)
            | (127, _, _, _)
            | (169, 254, _, _)
            | (172, 16..=31, _, _)
            | (192, 0, 0, _)
            | (192, 0, 2, _)
            | (192, 88, 99, _)
            | (192, 168, _, _)
            | (198, 18..=19, _, _)
            | (198, 51, 100, _)
            | (203, 0, 113, _)
            | (224..=255, _, _, _)
    )
}

pub(crate) fn is_public_ipv6(addr: Ipv6Addr) -> bool {
    if let Some(mapped) = addr.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    if addr.is_loopback() || addr.is_unspecified() || addr.is_multicast() {
        return false;
    }
    let segments = addr.segments();
    let first = segments[0];
    let second = segments[1];
    if (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80 {
        return false;
    }
    if first == 0x2001 && second == 0x0db8 {
        return false;
    }
    (first & 0xe000) == 0x2000
}

pub(crate) fn media_url_contains_hash(url: &reqwest::Url, expected_hash: &str) -> bool {
    url.path().as_bytes().windows(64).any(|window| {
        std::str::from_utf8(window)
            .ok()
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(expected_hash))
    })
}

/// Run an `upload_media` request, transparently self-healing a group whose
/// encrypted-media policy component predates darkmatter #319 (the endpoint
/// byte-layout change). Such components no longer decode under the strict
/// decoder — the policy reads back with an empty `media_format` and the upload
/// fails with "encrypted media format must be encrypted-media-v1". On that
/// class of failure we re-publish the policy with the current encoding (an MLS
/// commit — needs admin rights) and retry the upload once. Best-effort: if the
/// heal or retry fails we surface the original error. Shared by the single-file
/// and album upload paths.
pub(crate) async fn upload_media_with_heal(
    runtime: MarmotAppRuntime,
    label: String,
    group_id: GroupId,
    request: MediaUploadRequest,
) -> Result<MediaUploadResult> {
    match runtime
        .upload_media(&label, &group_id, request.clone())
        .await
    {
        Ok(r) => Ok(r),
        Err(e) => {
            let msg = e.to_string();
            if is_stale_encrypted_media_policy(&msg) {
                tracing::warn!(
                    target: "backend::upload_media",
                    error = %msg,
                    "encrypted-media policy is stale (pre-#319 layout); re-publishing endpoints and retrying"
                );
                let endpoints = vec![AppBlobEndpoint {
                    locator_kind: BLOSSOM_LOCATOR_KIND_V1.to_owned(),
                    base_url: DEFAULT_BLOSSOM_SERVER_URL.to_owned(),
                }];
                match runtime
                    .replace_encrypted_media_blob_endpoints(&label, &group_id, endpoints)
                    .await
                {
                    Ok(_) => runtime
                        .upload_media(&label, &group_id, request)
                        .await
                        .map_err(|e| anyhow!("upload_media (after policy heal): {e}")),
                    Err(heal) => {
                        tracing::warn!(
                            target: "backend::upload_media",
                            error = %heal,
                            "could not re-publish encrypted-media policy (not admin?)"
                        );
                        Err(anyhow!("upload_media: {e}"))
                    }
                }
            } else {
                Err(anyhow!("upload_media: {e}"))
            }
        }
    }
}

pub(crate) fn short_account_id(account_id_hex: &str) -> String {
    if account_id_hex.len() <= 12 {
        return account_id_hex.to_string();
    }
    format!("0x{}…", &account_id_hex[..8])
}

/// Build a placeholder directory record for a followed account whose profile
/// hasn't been resolved yet (no relay sync has populated the cache).
pub(crate) fn stub_directory_entry(account_id_hex: &str) -> UserDirectoryRecord {
    use marmot_app::{AccountRelayListState, AccountRelayListStatus};
    let empty_state = |kind: u64| AccountRelayListState {
        kind,
        relays: Vec::new(),
    };
    UserDirectoryRecord {
        account_id_hex: account_id_hex.to_string(),
        npub: marmot_app::npub_for_account_id(account_id_hex)
            .unwrap_or_else(|_| account_id_hex.to_string()),
        local_account: None,
        profile: None,
        follows: Vec::new(),
        follow_source_relays: Vec::new(),
        relay_lists: AccountRelayListStatus {
            complete: false,
            missing: Vec::new(),
            default_relays: Vec::new(),
            bootstrap_relays: Vec::new(),
            nip65: empty_state(10_002),
            inbox: empty_state(10_050),
        },
        key_package: None,
    }
}

/// Read relay URLs from the user's config dir. No defaults — empty list when
/// the file is missing or malformed, which is the documented behavior.
pub fn load_relays() -> Vec<String> {
    let Some(proj) = directories::ProjectDirs::from("", "", "darkmatter-linux") else {
        return Vec::new();
    };
    let path = proj.config_dir().join("relays.json");
    let Ok(bytes) = std::fs::read(&path) else {
        return Vec::new();
    };
    serde_json::from_slice::<Vec<String>>(&bytes).unwrap_or_default()
}

/// Persist the relay list. Best-effort — surfaces an error string on failure.
pub fn save_relays(relays: &[String]) -> Result<(), String> {
    let proj = directories::ProjectDirs::from("", "", "darkmatter-linux")
        .ok_or_else(|| "no config dir".to_string())?;
    let dir = proj.config_dir();
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join("relays.json");
    let bytes = serde_json::to_vec_pretty(relays).map_err(|e| e.to_string())?;
    std::fs::write(&path, bytes).map_err(|e| e.to_string())
}
