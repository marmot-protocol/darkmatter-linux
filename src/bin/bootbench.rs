//! Headless boot-path benchmark. Exercises the exact `Backend::boot` +
//! post-boot read sequence that `main.rs` performs after unlock, with phase
//! timings, so boot latency can be measured and bisected without driving the
//! GUI.
//!
//! Usage:
//!   DM_HOME=/tmp/dmbench BENCH_RELAYS=wss://relay.primal.net,wss://relay.ditto.pub \
//!     cargo run --bin bootbench
//!
//! The first run against an empty `DM_HOME` takes the first-run path
//! (blocking start + login — expected to be slow). Run it twice: the second
//! run measures the already-present path the real app takes after unlock.
//! The vault password is fixed (`bench-password`); the nsec is generated on
//! first run and read back from the vault afterwards.

#![allow(dead_code)]

#[path = "../backend.rs"]
mod backend;
#[path = "../blossom.rs"]
mod blossom;
#[path = "../media_cache.rs"]
mod media_cache;
#[path = "../observability.rs"]
mod observability;
#[path = "../vault.rs"]
mod vault;

// `vault.rs` is shared with the main binary, whose crate root owns this lock to
// serialize DM_HOME-rebinding tests. The benchmark pulls vault.rs in via
// `#[path]`, so its `#[cfg(test)]` module needs the same symbol at this crate
// root to compile under `cargo test` / `clippy --all-targets`.
#[cfg(test)]
pub(crate) static DM_HOME_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

use std::sync::{Arc, Mutex};
use std::time::Instant;

const PW: &str = "bench-password";

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    if std::env::var("DM_HOME").is_err() {
        eprintln!("set DM_HOME to a scratch dir first (refusing to touch the real account)");
        std::process::exit(2);
    }
    let relays: Vec<String> = std::env::var("BENCH_RELAYS")
        .map(|v| v.split(',').map(str::to_string).collect())
        .unwrap_or_else(|_| backend::load_relays());
    eprintln!(
        "[bench] home={:?} relays={relays:?}",
        backend::default_home()
    );

    let t = Instant::now();
    let (vault, nsec) = if vault::exists() {
        let v = vault::Vault::open(PW).expect("open bench vault");
        let nsec = v.nsec().expect("bench vault has nsec");
        (v, nsec)
    } else {
        use nostr::ToBech32;
        let keys = nostr::Keys::generate();
        let nsec = keys.secret_key().to_bech32().unwrap();
        let mut v = vault::Vault::create(PW).expect("create bench vault");
        v.set(vault::NSEC_KEY, &nsec).expect("seal nsec");
        (v, nsec)
    };
    eprintln!("[bench] vault ready in {:?}", t.elapsed());

    let secret_store = Arc::new(vault::VaultSecretStore::new(Arc::new(Mutex::new(vault))));
    let (tx, rx) = std::sync::mpsc::channel();
    let t_boot = Instant::now();
    let b = backend::Backend::boot(
        &nsec,
        relays,
        secret_store,
        None,
        move |r| {
            let _ = tx.send(r);
        },
        None,
    )
    .expect("boot");
    eprintln!("[bench] ── boot returned in {:?} ──", t_boot.elapsed());

    // Same read sequence the boot-return closure runs on the UI thread.
    fn step(name: &str, f: impl FnOnce()) {
        let t = Instant::now();
        f();
        eprintln!("[bench] {name}: {:?}", t.elapsed());
    }
    let mut chats = Vec::new();
    step("chats()", || chats = b.chats().unwrap_or_default());
    eprintln!("[bench]   {} chats", chats.len());
    step("latest_message() x chats", || {
        for c in &chats {
            let _ = b.latest_message(&c.group_id_hex);
        }
    });
    if let Some(first) = chats.first() {
        step("messages(first, 200)", || {
            let _ = b.messages(&first.group_id_hex, Some(200));
        });
        step("group_member_count(first)", || {
            let _ = b.group_member_count(&first.group_id_hex);
        });
        step("group_members(first)", || {
            let _ = b.group_members(&first.group_id_hex);
        });
    }
    step("archived_chats()", || {
        let _ = b.archived_chats();
    });
    step("follow_list()", || {
        let _ = b.follow_list();
    });
    step("load_profile()", || {
        let _ = b.load_profile();
    });
    step("key_packages_local()", || {
        let _ = b.key_packages_local();
    });
    step("key_package_relays()", || {
        let _ = b.key_package_relays();
    });
    step("relay_health()", || {
        let _ = b.relay_health();
    });
    step("telemetry/audit flags", || {
        let _ = b.telemetry_enabled();
        let _ = b.audit_logs_enabled();
    });
    eprintln!(
        "[bench] ── interactive after {:?} from boot start ──",
        t_boot.elapsed()
    );

    match rx.recv() {
        Ok(r) => eprintln!(
            "[bench] background sync done at {:?} from boot start: {:?}",
            t_boot.elapsed(),
            r.map(|_| "ok")
        ),
        Err(_) => eprintln!("[bench] sync channel closed without a result"),
    }
    // What the on_synced refresh costs (merge_chat_list_rows reads).
    step("chats() post-sync", || {
        let _ = b.chats();
    });
    step("relay_health() post-sync", || {
        let _ = b.relay_health();
    });
}
