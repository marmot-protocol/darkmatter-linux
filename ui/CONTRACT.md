# The View-Model Contract

This is the **interface between `src/main.rs` (the glue) and the Slint UI tree**. It
is the keystone of the theming engine: a *skin* (a drastically different rendering of
the app) is an interchangeable consumer of this contract. The whole engine works only
because of one rule.

## The Rule

> **A skin may only *consume* contract data (the structs + the root component's
> in-properties) and *invoke* the contract sinks (sink globals + the root component's
> callbacks). A skin may NEVER require new glue in `main.rs` or `backend.rs`.**

If a new theme/skin needs a value or an action that the contract doesn't already
expose, that is a contract change — it must be added here and wired in `main.rs`
*once*, for *all* skins — never as a one-off binding that only one skin uses.

This is what guarantees that adding a radically different-looking theme touches no
Rust glue and no other theme's code (ISC #1), and that the MarmotApp data flow keeps
working under every theme (ISC #3).

## The contract surface

### 1. Data structs (`ui/tokens.slint`)

The view-model. Rust constructs these directly and pushes them to the root. Skins read
them; skins never invent their own message/contact/chat model.

`ChatMessage`, `MessageLine`, `MessageRun`, `AlbumCell`, `Reaction` · `ChatMeta` ·
`Contact` · `GroupMember` · `AccountEntry` · `ArchivedChat` · `StagedAttachment` ·
`EditVersion` · `EmojiEntry` · `EffectChoice` · `AuditLogEntry`.
(Also `keys/types.slint`: `Signer`, `LinkedDevice`, `KeyPackageInfo`;
`modals/command-palette.slint`: `PaletteAction`.)

These structs are **frozen** with respect to skins: a skin must render whatever subset
of fields it wants and ignore the rest. It must not depend on a field being absent.

### 2. Sink globals (`ui/tokens.slint`)

Leaf-to-Rust channels that bypass callback threading. A skin invokes these directly:

- `Linkout.open(url)` — a tapped Markdown anchor.
- `ProfileSink.open(account_id_hex)` — a tapped avatar / sender name.
- `EmojiSheet.sprite` / `.tile` — the shared Twemoji texture (read-only).
- `EffectCatalog.choices` / `.selected` — message-effect catalog + pending selection.

### 3. Root component interface (`ui/dark-matter-linux.slint`)

The `DarkMatterLinux` root exposes ~184 properties and ~113 callbacks that `main.rs`
binds once. This is the authoritative action/state surface. Skins and shells route
user intent through these callbacks (e.g. `send-message`, `react-message`,
`request-reply`, `attachment-clicked`, `switch-account`) and read state from these
properties (e.g. `composer-draft`, `chats`, `messages`, `reply-target-*`).

### 4. Theme-selection properties (managed by the engine, set from `settings.rs`)

The only theme state Rust sets. Today: `light-theme: bool`, `retro-mode: bool`,
`accent-color: int`. **Phase B generalizes these into a single `theme-id: int`** (plus
`accent-color`), which selects a `ThemeTokens` pack and, transitively, the per-family
skin ids and shell id. Rust's job shrinks to "set the active theme id + accent"; all
resolution happens in Slint.

## What a skin is allowed to do

- Read any contract struct field and render it however it likes (or not at all).
- Read any `Tokens.*` value (Phase B) — colors, type sizes, geometry, motion, flags.
- Invoke any sink global or root callback.
- Mount its own internal sub-components, animations, and layout freely.

## What a skin must NOT do

- Add a new property/callback that only it consumes (→ contract change instead).
- Reach into `MarmotApp`/`Backend` or assume anything about data production.
- Branch on theme *identity* (`Theme.retro`, `theme-id == 3`) for styling. Style comes
  from `Tokens.*`; structure comes from being *selected* as the active skin. (A skin
  body is chosen by the dispatch slot — it never asks "am I the active theme?".)

## Layers built on this contract

```
Theme (id + accent)          ← Rust sets only this
  └─ Tokens (ThemeTokens)    ← L0: colors/type/geometry/motion/flags  (Phase B)
  └─ component skin slots    ← L1: dispatch to a skin body per family  (Phase C)
  └─ shell variant slot      ← L2: dispatch to a shell skeleton        (Phase D)
  └─ (future) runtime plugins via slint-interpreter — contract permits, not built
```

## How to add a theme

Everything lives in `ui/tokens.slint` + two tiny selection sites. A recolor is one
struct; a drastic theme additionally writes skin bodies and bumps the selectors.

1. **Colors** — append a `ThemeColors` literal to `Theme.color-packs` (the new
   index is the theme id).
2. **Personality** — append a parallel `ThemeStyle` literal to `Theme.style-packs`:
   the capability flags (`hard-shadow`, `bevel`, `pixel-icons`, `bracket-labels`, …)
   and the per-family **skin selectors** (`skin-message`, `skin-list`, … `shell`).
   For a pure recolor, copy an existing pack's flags and keep all selectors `0`.
3. **Skins (only if a selector is non-zero)** — add the alternate body to that
   family's slot, guarded by its selector value, reading the contract structs:
   - messages → `ui/primitives/message-view.slint` (`if Theme.skin-message == N`)
   - chat list → `ui/primitives/chat-list-entry.slint` (`if Theme.skin-list == N`)
   - shell → inline `if Theme.shell == N` skeleton in `ui/dark-matter-linux.slint`
4. **Make it selectable** — add the mode string in `normalize_theme_mode` /
   `apply_theme_mode` (`src/main.rs`), a matching `*-mode` bool threaded
   root → `settings-page.slint` → `appearance-pane.slint`, the `Theme.id` mapping
   in the root's `changed` handlers, and a toggle in the appearance pane.

The worked example is theme id 3, **Terminal** (terminal message lines + IRC chat
list + bracketed buttons): it required **zero** changes to message/list/button
*rendering logic in Rust* — only the theme-selection plumbing in step 4.
