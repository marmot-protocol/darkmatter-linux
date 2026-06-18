//! Headless control daemon + client for Dark Matter Linux.
//!
//! The GUI app has no automation surface, so this binary reuses the exact same
//! `Backend` (= MarmotApp + tokio runtime + vault) that `main.rs` drives, but
//! exposes it over a line-delimited JSON protocol on a Unix socket. That lets
//! the `dmvm` Ruby tool (or anything) create accounts, make groups, send
//! messages and flip settings without touching the GUI.
//!
//! Two modes, one binary:
//!   dm-ctl serve            boot the Backend once and listen on $DM_HOME/dmctl.sock
//!   dm-ctl <cmd> [args...]  connect to that socket, run one command, print JSON
//!
//! On first run `serve` creates the password-encrypted vault (password from
//! $DM_CTL_PW, default `darkmatter`), generates an nsec (or uses $DM_CTL_NSEC),
//! and — per policy — turns telemetry and audit logs ON by default.
//!
//! Commands (all args are strings, as they arrive from argv):
//!   ping
//!   whoami                              -> active account {label, account_id_hex, npub}
//!   accounts                            -> all local accounts
//!   account-add <nsec>                  -> add a second account
//!   flags                               -> {telemetry, audit}
//!   telemetry <on|off> ; audit <on|off>
//!   profile-get ; profile-set k=v ...   (name, display_name, about, picture, nip05, lud16)
//!   follow <npub|hex> ; follows
//!   group-create <name> [member ...]    -> {group_id_hex}
//!   group-list                          -> active (non-archived) chats
//!   group-members <group_hex>
//!   invite <group_hex> <member ...>
//!   rename <group_hex> <name>
//!   send <group_hex> <text...>          -> SendSummary
//!   messages <group_hex> [limit]
//!   react <group_hex> <msg_hex> <emoji>
//!   relays
//!   settings-get ; settings-set k=v ... (theme, locale, accent_color, debug_enabled, …)
//!   shutdown                            -> stop the daemon

#![allow(dead_code)]

#[path = "../backend.rs"]
mod backend;
#[path = "../blossom.rs"]
mod blossom;
#[path = "../media_cache.rs"]
mod media_cache;
#[path = "../observability.rs"]
mod observability;
#[path = "../settings.rs"]
mod settings;
#[path = "../vault.rs"]
mod vault;

// vault.rs's `#[cfg(test)]` module references this crate-root lock (it is shared
// with the main binary). Mirror it so `cargo test`/`clippy --all-targets` build.
#[cfg(test)]
pub(crate) static DM_HOME_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use backend::Backend;

fn sock_path() -> PathBuf {
    backend::default_home().join("dmctl.sock")
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = if args.is_empty() {
        "help".to_string()
    } else {
        args.remove(0)
    };

    match cmd.as_str() {
        "serve" => {
            if let Err(e) = serve() {
                eprintln!("dm-ctl serve: {e:#}");
                std::process::exit(1);
            }
        }
        "help" | "-h" | "--help" => {
            eprintln!("usage: dm-ctl serve | dm-ctl <command> [args...]  (see file header)");
        }
        _ => {
            // Client mode: relay one command to the running daemon.
            std::process::exit(client(&cmd, &args));
        }
    }
}

// ---- client -------------------------------------------------------------

fn client(cmd: &str, args: &[String]) -> i32 {
    let req = json!({ "cmd": cmd, "args": args });
    let path = sock_path();
    let stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "dm-ctl: can't reach daemon at {} ({e}). Start it with `dm-ctl serve`.",
                path.display()
            );
            return 3;
        }
    };
    let mut writer = stream.try_clone().expect("clone socket");
    let mut reader = BufReader::new(stream);
    if writeln!(writer, "{req}").and_then(|_| writer.flush()).is_err() {
        eprintln!("dm-ctl: failed to send request");
        return 3;
    }

    // `watch` streams: echo every JSON line the daemon pushes until it (or we)
    // close. A consumer (e.g. `dmvm dm watch`) formats these for the terminal.
    if cmd == "watch" {
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    print!("{line}");
                    let _ = std::io::stdout().flush();
                }
            }
        }
        return 0;
    }

    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        eprintln!("dm-ctl: daemon closed the connection without replying");
        return 3;
    }
    match serde_json::from_str::<Value>(&line) {
        Ok(v) => {
            let ok = v.get("ok").and_then(Value::as_bool).unwrap_or(false);
            if ok {
                let result = v.get("result").cloned().unwrap_or(Value::Null);
                println!("{}", serde_json::to_string_pretty(&result).unwrap());
                0
            } else {
                let err = v.get("error").and_then(Value::as_str).unwrap_or("unknown error");
                eprintln!("dm-ctl: {err}");
                1
            }
        }
        Err(_) => {
            print!("{line}");
            0
        }
    }
}

// ---- daemon -------------------------------------------------------------

fn serve() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    let pw = std::env::var("DM_CTL_PW").unwrap_or_else(|_| "darkmatter".to_string());
    let relays: Vec<String> = std::env::var("BENCH_RELAYS")
        .ok()
        .map(|v| v.split(',').map(str::to_string).collect())
        .unwrap_or_else(backend::load_relays);

    let first_run = !vault::exists();
    let (vault_obj, nsec) = if vault::exists() {
        let v = vault::Vault::open(&pw).context("open vault (wrong DM_CTL_PW?)")?;
        let nsec = v.nsec().ok_or_else(|| anyhow!("vault has no nsec"))?;
        (v, nsec)
    } else {
        use nostr::ToBech32;
        let nsec = match std::env::var("DM_CTL_NSEC") {
            Ok(n) => n,
            Err(_) => nostr::Keys::generate()
                .secret_key()
                .to_bech32()
                .expect("encode nsec"),
        };
        let mut v = vault::Vault::create(&pw).context("create vault")?;
        v.set(vault::NSEC_KEY, &nsec).context("seal nsec")?;
        (v, nsec)
    };
    eprintln!("[dm-ctl] vault ready (first_run={first_run})");

    let secret_store = Arc::new(vault::VaultSecretStore::new(Arc::new(Mutex::new(vault_obj))));
    let backend = Backend::boot(&nsec, relays, secret_store, None, |_r| {}, None)
        .context("boot backend")?;
    eprintln!("[dm-ctl] backend booted, account {}", backend.account().account_id_hex);

    // Policy: telemetry + audit logs ON by default on a freshly created identity.
    if first_run {
        if let Err(e) = backend.set_telemetry_enabled(true) {
            eprintln!("[dm-ctl] could not enable telemetry: {e}");
        }
        if let Err(e) = backend
            .tokio_handle()
            .block_on(backend.set_audit_logs_enabled(true))
        {
            eprintln!("[dm-ctl] could not enable audit logs: {e}");
        }
        eprintln!("[dm-ctl] telemetry + audit logs enabled by default");
    }

    let path = sock_path();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?;
    eprintln!("[dm-ctl] listening on {}", path.display());

    for conn in listener.incoming() {
        let stream = match conn {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[dm-ctl] accept error: {e}");
                continue;
            }
        };
        if handle_conn(stream, &backend) {
            eprintln!("[dm-ctl] shutdown requested");
            break;
        }
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}

/// Returns true if the daemon should shut down.
fn handle_conn(stream: UnixStream, backend: &Backend) -> bool {
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return false,
    };
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return false;
    }

    let req: Value = match serde_json::from_str(&line) {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(writer, "{}", json!({"ok": false, "error": format!("bad request: {e}")}));
            return false;
        }
    };
    let cmd = req.get("cmd").and_then(Value::as_str).unwrap_or("").to_string();
    let args: Vec<String> = req
        .get("args")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();

    // `watch` is a long-lived stream, not a single request/reply: hold the
    // connection open and push one JSON line per incoming message until the
    // client disconnects.
    if cmd == "watch" {
        stream_watch(reader, writer, backend, &args);
        return false;
    }

    let shutdown = cmd == "shutdown";
    let reply = match dispatch(backend, &cmd, &args) {
        Ok(result) => json!({ "ok": true, "result": result }),
        Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
    };
    let _ = writeln!(writer, "{reply}");
    let _ = writer.flush();
    shutdown
}

/// Stream live messages for one group to the client until it disconnects.
fn stream_watch(
    mut reader: BufReader<UnixStream>,
    mut writer: UnixStream,
    backend: &Backend,
    args: &[String],
) {
    let group = match args.first() {
        Some(g) => g.clone(),
        None => {
            let _ = writeln!(writer, "{}", json!({"ok": false, "error": "watch needs a group_hex"}));
            return;
        }
    };

    let (tx, rx) = std::sync::mpsc::channel::<Value>();
    let filter = group.clone();
    let handle = backend.watch_messages(&group, move |update| {
        let m = update.message();
        let g = hex::encode(m.group_id.as_slice());
        if !g.eq_ignore_ascii_case(&filter) {
            return; // marmot's dedup can re-emit other groups' events
        }
        let _ = tx.send(json!({
            "type": "message",
            "kind": m.kind,
            "message_id": m.message_id_hex,
            "group_id_hex": g,
            "sender": m.sender,
            "sender_name": m.sender_display_name,
            "text": m.plaintext,
            "tags": m.tags,
            "recorded_at": m.recorded_at,
        }));
    });

    // Detect client disconnect: the client never sends more data, so a 0-byte
    // read means the socket closed.
    let closed = Arc::new(AtomicBool::new(false));
    {
        let closed = closed.clone();
        std::thread::spawn(move || {
            let mut buf = String::new();
            loop {
                buf.clear();
                match reader.read_line(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
            closed.store(true, Ordering::SeqCst);
        });
    }

    // Confirm the subscription is live so the client can print a banner.
    let _ = writeln!(writer, "{}", json!({"type": "ready", "group_id_hex": group}));
    let _ = writer.flush();

    loop {
        if closed.load(Ordering::SeqCst) {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(v) => {
                if writeln!(writer, "{v}").and_then(|_| writer.flush()).is_err() {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    handle.abort();
}

// ---- command dispatch ---------------------------------------------------

fn arg<'a>(args: &'a [String], i: usize, name: &str) -> Result<&'a str> {
    args.get(i).map(String::as_str).ok_or_else(|| anyhow!("missing argument: {name}"))
}

fn parse_bool(s: &str) -> Result<bool> {
    match s.to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "yes" | "enable" | "enabled" => Ok(true),
        "off" | "false" | "0" | "no" | "disable" | "disabled" => Ok(false),
        _ => Err(anyhow!("expected on/off, got '{s}'")),
    }
}

/// Split "k=v" tokens into pairs.
fn kv_pairs(args: &[String]) -> Vec<(String, String)> {
    args.iter()
        .filter_map(|a| a.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect()
}

/// `SendSummary` (from marmot) isn't `Serialize`, so hand-roll it.
fn send_json(s: marmot_app::SendSummary) -> Value {
    json!({ "published": s.published, "message_ids": s.message_ids })
}

fn npub_of(hex_id: &str) -> String {
    use nostr::ToBech32;
    nostr::PublicKey::parse(hex_id)
        .ok()
        .and_then(|pk| pk.to_bech32().ok())
        .unwrap_or_default()
}

fn dispatch(backend: &Backend, cmd: &str, args: &[String]) -> Result<Value> {
    match cmd {
        "ping" => Ok(json!("pong")),

        "whoami" => {
            let a = backend.account();
            Ok(json!({
                "label": a.label,
                "account_id_hex": a.account_id_hex,
                "npub": npub_of(&a.account_id_hex),
            }))
        }

        "accounts" => {
            let list: Vec<Value> = backend
                .accounts()
                .into_iter()
                .map(|a| json!({
                    "label": a.label,
                    "account_id_hex": a.account_id_hex.clone(),
                    "npub": npub_of(&a.account_id_hex),
                    "local_signing": a.local_signing,
                }))
                .collect();
            Ok(json!(list))
        }

        "account-add" => {
            let nsec = arg(args, 0, "nsec")?.to_string();
            // add_account_async is fire-and-forget; bridge it to sync via a channel.
            let (tx, rx) = std::sync::mpsc::channel();
            backend.add_account_async(nsec, move |r| {
                let _ = tx.send(r.map(|a| a.account_id_hex));
            });
            let id = rx
                .recv()
                .map_err(|_| anyhow!("worker dropped"))??;
            Ok(json!({ "account_id_hex": id.clone(), "npub": npub_of(&id) }))
        }

        "flags" => Ok(json!({
            "telemetry": backend.telemetry_enabled(),
            "audit": backend.audit_logs_enabled(),
        })),

        "telemetry" => {
            let on = parse_bool(arg(args, 0, "on|off")?)?;
            backend.set_telemetry_enabled(on)?;
            Ok(json!({ "telemetry": on }))
        }

        "audit" => {
            let on = parse_bool(arg(args, 0, "on|off")?)?;
            backend
                .tokio_handle()
                .block_on(backend.set_audit_logs_enabled(on))?;
            Ok(json!({ "audit": on, "note": "takes effect on next backend restart" }))
        }

        "profile-get" => Ok(serde_json::to_value(backend.load_profile()?)?),

        "profile-set" => {
            let mut p = backend.load_profile()?.unwrap_or_default();
            for (k, v) in kv_pairs(args) {
                let val = Some(v.clone());
                match k.as_str() {
                    "name" => p.name = val,
                    "display_name" | "display-name" => p.display_name = val,
                    "about" => p.about = val,
                    "picture" => p.picture = val,
                    "nip05" => p.nip05 = val,
                    "lud16" => p.lud16 = val,
                    other => return Err(anyhow!("unknown profile field '{other}'")),
                }
            }
            Ok(serde_json::to_value(backend.save_profile(p)?)?)
        }

        "follow" => {
            let who = arg(args, 0, "npub|hex")?;
            Ok(json!({ "followed": backend.add_contact(who)? }))
        }

        "follows" => Ok(serde_json::to_value(backend.follow_list()?)?),

        "group-create" => {
            let name = arg(args, 0, "name")?;
            let members: Vec<String> = args.iter().skip(1).cloned().collect();
            let gid = backend.create_group(name, &members)?;
            Ok(json!({ "group_id_hex": hex::encode(gid.as_slice()) }))
        }

        "group-list" => Ok(serde_json::to_value(backend.chats()?)?),

        "group-members" => {
            let g = arg(args, 0, "group_hex")?;
            Ok(serde_json::to_value(backend.group_members(g)?)?)
        }

        "invite" => {
            let g = arg(args, 0, "group_hex")?;
            let members: Vec<String> = args.iter().skip(1).cloned().collect();
            if members.is_empty() {
                return Err(anyhow!("invite needs at least one member"));
            }
            Ok(send_json(backend.invite_members(g, &members)?))
        }

        "rename" => {
            let g = arg(args, 0, "group_hex")?;
            let name = arg(args, 1, "name")?;
            Ok(send_json(backend.rename_group(g, name)?))
        }

        "send" => {
            let g = arg(args, 0, "group_hex")?;
            let text = args.get(1..).map(|s| s.join(" ")).unwrap_or_default();
            if text.is_empty() {
                return Err(anyhow!("send needs message text"));
            }
            Ok(send_json(backend.send_text(g, &text)?))
        }

        "messages" => {
            let g = arg(args, 0, "group_hex")?;
            let limit = args.get(1).and_then(|s| s.parse::<usize>().ok());
            Ok(serde_json::to_value(backend.messages(g, limit)?)?)
        }

        "react" => {
            let g = arg(args, 0, "group_hex")?;
            let m = arg(args, 1, "message_hex")?;
            let emoji = arg(args, 2, "emoji")?;
            Ok(send_json(backend.react(g, m, emoji)?))
        }

        "relays" => Ok(json!(backend.booted_relays())),

        "settings-get" => Ok(serde_json::to_value(settings::Settings::load())?),

        "settings-set" => {
            let mut s = settings::Settings::load();
            for (k, v) in kv_pairs(args) {
                match k.as_str() {
                    "theme" => s.theme = v,
                    "locale" => s.locale = v,
                    "accent_color" | "accent" => s.accent_color = v,
                    "time_format" => s.time_format = v,
                    "date_format" => s.date_format = v,
                    "debug_enabled" | "debug" => s.debug_enabled = parse_bool(&v)?,
                    "outgoing_on_right" => s.outgoing_on_right = parse_bool(&v)?,
                    "notifications_enabled" => s.notifications_enabled = parse_bool(&v)?,
                    "notification_sound" => s.notification_sound = parse_bool(&v)?,
                    "notification_preview" => s.notification_preview = parse_bool(&v)?,
                    other => return Err(anyhow!("unknown setting '{other}'")),
                }
            }
            s.save();
            Ok(serde_json::to_value(s)?)
        }

        "shutdown" => Ok(json!("bye")),

        other => Err(anyhow!("unknown command '{other}'")),
    }
}
