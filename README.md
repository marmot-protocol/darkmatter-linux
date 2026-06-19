# Dark Matter for Linux

A native desktop client for [Marmot](https://github.com/marmot-protocol/darkmatter): end-to-end encrypted group chat over Nostr. Written in Rust with a [Slint](https://slint.dev) UI.

This is `v0.1.0`. It works, but it's early and moving fast, so expect rough edges.

## What it is

Marmot is [MLS](https://messaginglayersecurity.rocks/) group messaging carried over [Nostr](https://nostr.com) relays. That combination gives you the forward secrecy and post-compromise security of MLS together with a portable, self-owned Nostr identity: no phone number, no central server, no account anyone can take away from you. Dark Matter is a desktop front end for it.

The whole thing is one Rust binary. There's no OS keyring, no `pass`, and no plaintext key sitting on disk. Every secret (your nsec, Marmot's per-account MLS keys, the decrypted media cache) lives in a single vault file (`vault.db`) sealed with XChaCha20-Poly1305 under a key derived from your password with Argon2id. The flip side is that there's no recovery: lose the password and the data is gone.

## What works today

- One-to-one and group chats, end-to-end encrypted through Marmot's MLS, with sealed-sender invites over NIP-59.
- Multiple accounts at once. Each Nostr identity gets its own live Marmot worker, and switching accounts just changes what's on screen.
- Media: image album grids, inline video (via an embedded libmpv), and voice messages, all sent over Marmot's encrypted MIP-04 path. Profile pictures are the one exception, going out publicly via Blossom.
- Markdown message bodies (CommonMark + GFM + inline nostr entities), reactions, replies, and edits with history.
- Contacts and follow lists, private per-contact nicknames, an archive, and npub QR codes.
- Three themes (a modern dark, a warm light, and a full SNES-era retro skin with a pixel font) plus five accent colors.
- English, Italian, German, and Japanese, switchable at runtime.
- A durable on-disk send queue, so messages you write offline aren't lost and go out when you reconnect.
- Native desktop notifications.
- Opt-in OTLP metrics and audit logging, both off until you turn them on in Settings.

## Installing a release

Pre-built tarballs are on the [Releases](https://github.com/marmot-protocol/darkmatter-linux/releases) page:

| Platform | Target |
| --- | --- |
| Linux x86-64 | `x86_64-unknown-linux-gnu` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` |
| macOS (Apple Silicon) | `aarch64-apple-darwin` |

```sh
tar xzf darkmatter-linux-<target>.tar.gz
cd darkmatter-linux-<target>
./darkmatter-linux
```

You'll still need the runtime libraries listed under system dependencies below.

## Building from source

You need a current Rust toolchain (edition 2024) and a handful of C libraries for media, fonts, audio, and notifications.

On Debian or Ubuntu:

```sh
sudo apt-get install -y pkg-config libmpv-dev libfontconfig-dev libasound2-dev libdbus-1-dev
```

On macOS with Homebrew:

```sh
brew install mpv pkgconf
export PKG_CONFIG_PATH="$(brew --prefix)/lib/pkgconfig"
```

Then:

```sh
git clone https://github.com/marmot-protocol/darkmatter-linux
cd darkmatter-linux
cargo run
```

The first build takes a while, because it fetches the Marmot crates, compiles a very large generated Slint UI module, and composes the Twemoji sprite sheet. After that, incremental builds are quick. Editing Rust under `src/` only rebuilds the root crate (a couple of seconds), while touching `.slint` or `lang/` files rebuilds the UI crate (around 25 seconds). The Marmot crates are pulled anonymously over HTTPS from the public [`marmot-protocol/darkmatter`](https://github.com/marmot-protocol/darkmatter) repo, so there's no SSH key or token to set up.

### First run

The first time you launch, you either paste an existing nsec or generate a new one, and you set a vault password. That creates the vault. From then on you just enter the password to open it. A wrong password fails the cipher's authentication tag, so there's no recovery path, but the login screen has a "Use another key" option that wipes the vault and starts over from a fresh nsec.

## Configuration

A few environment variables matter at runtime:

| Variable | Effect |
| --- | --- |
| `DM_HOME` | Where the vault, media cache, and observability override live. Defaults to the platform's standard data directory for `darkmatter`. |
| `RUST_LOG` | `tracing` filter; logs go to stderr, defaulting to `info`. |
| `WAYLAND_DISPLAY` / `DISPLAY` | Selects the clipboard backend, preferring `wl-copy` on Wayland and falling back to `xclip`, `xsel`, or `arboard` on X11. |

UI preferences (theme, accent, locale, which side your own messages sit on, nicknames) are kept in a small JSON file in your XDG config directory. Telemetry and audit-log endpoints are configured in `observability.toml`, but nothing is ever sent until you enable the toggles under Settings, in the Advanced section.

## How the code is laid out

The architecture, top to bottom:

```
 Slint UI (ui/*.slint)
        │   compiled once into a generated module
        ▼
   dm-ui crate   : owns slint::include_modules!()
        │
        ▼
   src/main.rs   : the callback glue and the optimistic-overlay state machine
        │
        ▼
  src/backend.rs : wraps MarmotApp and its own tokio runtime
        │
        ▼
  MarmotApp       : MLS groups, Nostr relays, sealed secrets
```

It's deliberately flat. There are no `send` / `react` / `members` submodules; the full data flow for any given UI action reads straight down `main.rs`. One real split exists, the `dm-ui` crate, and it's there for a single practical reason: the generated Slint module is enormous, so isolating it keeps everyday Rust edits from triggering a full UI recompile.

| Path | What's there |
| --- | --- |
| `src/main.rs` | UI callback wiring, the chat-row build pipeline, the optimistic overlay |
| `src/backend.rs` | The `MarmotApp` wrapper, the tokio runtime, and all platform-specific bits (clipboard, paths) |
| `src/vault.rs` | The password-encrypted secret vault |
| `src/media_cache.rs` | Encrypted-at-rest cache for decrypted attachment bytes |
| `src/blossom.rs` | Public Blossom uploads, used only for profile pictures |
| `src/mpv.rs`, `src/audio.rs` | Inline video over libmpv, and voice-message capture and playback |
| `src/offline_queue.rs` | The durable outgoing-message queue |
| `src/animal_avatar.rs` | Deterministic starter avatars drawn over an npub-derived gradient |
| `src/settings.rs`, `src/observability.rs`, `src/notify.rs` | UI prefs, telemetry config, desktop notifications |
| `dm-ui/` | The build-isolation crate that compiles the Slint tree and the emoji sprite sheet |
| `ui/` | The Slint component tree; `tokens.slint` holds the shared structs and theme globals, and `ui/CONTRACT.md` documents the theming engine |
| `lang/` | gettext catalogs (`en`, `it`, `de`, `ja`), bundled at build time |
| `assets/` | Logo, fonts, and the SVG starter-avatar set |

A few design choices are worth knowing before you dig in:

- **Optimistic rendering.** Sending, reacting, and unreacting apply to a local overlay first and repaint immediately, then reconcile against Marmot's response. The UI never blocks on the network round-trip.
- **Two upload paths.** Chat attachments go through Marmot's encrypted MIP-04 path, readable only by group members. Profile pictures take the deliberately public Blossom path.
- **Three-way theming.** Every color token branches across modern, light, and retro. A new component has to cover all three and read accent colors from the `Theme` global instead of hardcoding them.

For the deeper details (vault format, the avatar pipeline, the i18n setup, and the Slint conventions specific to this repo), see [`AGENTS.md`](AGENTS.md).

## Working on it

```sh
cargo build                      # build everything
cargo run                        # run the app
cargo run --bin bootbench        # boot benchmark
scripts/update-translations.sh   # regenerate gettext catalogs after editing @tr() strings
```

There's no automated test suite yet. `cargo test` is a no-op, and changes are checked by running the app. To develop against a local Marmot checkout, add a `[patch]` to `.cargo/config.toml` instead of editing `Cargo.toml`; the exact stanza is in `AGENTS.md`.

## Contributing

Issues and pull requests are welcome. If you're working with an AI coding agent, point it at [`AGENTS.md`](AGENTS.md) first, since it has the architecture and the conventions in more detail than this file does.

Before your first commit, install the project git hooks:

```sh
scripts/install-hooks.sh
```

This is required. It points `core.hooksPath` at the tracked `.githooks/` directory and registers the `po-clean` catalog filter, so the same checks CI enforces run locally before you commit. The `pre-commit` hook:

- **Normalizes gettext catalogs** (`*.po` / `*.pot`) — it strips source-line references and the volatile `POT-Creation-Date` header and sorts by message id, so unrelated line shifts never surface as catalog diffs or merge conflicts. This needs `gettext` (`msgcat`) installed; without it the hook still strips the date header. The same normalization runs automatically on `git add` via the filter, and CI rejects any catalog that isn't normalized.
- **Gates Rust/Slint/Cargo changes** on `cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings` — the same gates CI runs. Run `cargo fmt --all` to fix formatting before committing.

In a genuine emergency you can bypass a single commit with `git commit --no-verify`, but CI runs the same checks, so the bypass only defers them.

## License

Licensed under the GNU Affero General Public License, version 3 (AGPL-3.0). See [`LICENSE`](LICENSE) for the full text. In short: you're free to use, modify, and redistribute it, but if you run a modified version as a network service you have to offer your users its source.
