use crate::*;

/// Cheap deterministic avatar palette + initials from any string key.
pub(crate) fn avatar_for(key: &str) -> (Color, Color, String) {
    let mut h: u32 = 0x811c_9dc5;
    for b in key.as_bytes() {
        h = h.wrapping_mul(16_777_619) ^ (*b as u32);
    }
    let hue_a = 0x80_8080u32.wrapping_add(h & 0x7f_7f7f);
    let hue_b = 0x20_2020u32.wrapping_add(h.rotate_left(7) & 0x3f_3f3f);
    let init: String = key
        .split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase();
    let init = if init.is_empty() {
        key.chars().take(2).collect::<String>().to_uppercase()
    } else {
        init
    };
    (rgb(hue_a), rgb(hue_b), init)
}

pub(crate) fn short_hex(s: &str) -> String {
    if s.len() <= 6 {
        s.to_string()
    } else {
        s[..6].to_string()
    }
}

// ─── Emoji catalog ──────────────────────────────────────────────────────

// `emoji_sprite_map` and `EMOJI_SPRITE_PNG` come from wnl-ui (via the glob
// import at the top) — they're emitted by that crate's build.rs.

// No cap: the picker grid in Slint manually virtualizes (only rows whose
// y-range intersects the viewport are instantiated), so the full ~1900-emoji
// catalog stays cheap regardless of how many match.

/// Decode the build-time sprite sheet into a `slint::Image`. Cached so
/// repeated calls reuse the same texture.
pub(crate) fn emoji_sprite_image() -> slint::Image {
    use std::cell::RefCell;
    thread_local! {
        static CACHE: RefCell<Option<slint::Image>> = const { RefCell::new(None) };
    }
    if let Some(img) = CACHE.with(|c| c.borrow().clone()) {
        return img;
    }
    let decoded = image::load_from_memory_with_format(EMOJI_SPRITE_PNG, image::ImageFormat::Png)
        .expect("decode embedded twemoji sprite")
        .to_rgba8();
    let (w, h) = decoded.dimensions();
    let buffer =
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(decoded.as_raw(), w, h);
    let image = slint::Image::from_rgba8(buffer);
    CACHE.with(|c| *c.borrow_mut() = Some(image.clone()));
    image
}

/// Build a fast lookup from emoji string → its (x, y) position in the
/// sprite sheet, using the table emitted by build.rs.
pub(crate) fn emoji_position_index() -> &'static std::collections::HashMap<&'static str, (u32, u32)>
{
    use std::collections::HashMap;
    use std::sync::OnceLock;
    static IDX: OnceLock<HashMap<&'static str, (u32, u32)>> = OnceLock::new();
    IDX.get_or_init(|| {
        emoji_sprite_map::EMOJI_POSITIONS
            .iter()
            .map(|(e, x, y)| (*e, (*x, *y)))
            .collect()
    })
}

// ── Message effects (Telegram-style bursts) ─────────────────────────────────
//
// A small catalog of one-shot particle effects. Each is (catalog-id, wire-key,
// emoji). The id drives the Slint motion switch in message-effect-layer.slint;
// the wire-key is what travels in the body marker; the emoji is rendered as the
// flying particle (resolved to a sprite-sheet tile via the inline-emoji index).
pub(crate) const EFFECTS: &[(i32, &str, &str)] = &[
    (1, "love", "❤️"),
    (2, "fire", "🔥"),
    (3, "party", "🎉"),
    (4, "star", "⭐"),
    (5, "like", "👍"),
];

/// Invisible delimiter wrapping the body effect marker. U+2063 (INVISIBLE
/// SEPARATOR) renders as nothing in conformant clients, so a non-DM client that
/// doesn't understand the marker just shows the clean body with a trailing
/// zero-width char.
pub(crate) const FX_MARK: char = '\u{2063}';

pub(crate) fn effect_key(id: i32) -> Option<&'static str> {
    EFFECTS.iter().find(|e| e.0 == id).map(|e| e.1)
}
pub(crate) fn effect_emoji(id: i32) -> Option<&'static str> {
    EFFECTS.iter().find(|e| e.0 == id).map(|e| e.2)
}
pub(crate) fn effect_id_from_key(key: &str) -> i32 {
    EFFECTS
        .iter()
        .find(|e| e.1 == key)
        .map(|e| e.0)
        .unwrap_or(0)
}

/// Resolve an effect's emoji to its (x, y) tile in the Twemoji sheet, tolerating
/// the presence/absence of a trailing U+FE0F variation selector (the sprite
/// index and the catalog string can disagree on it).
pub(crate) fn effect_clip(id: i32) -> Option<(u32, u32)> {
    let emoji = effect_emoji(id)?;
    let idx = emoji_position_index();
    if let Some(p) = idx.get(emoji) {
        return Some(*p);
    }
    let stripped = emoji.trim_end_matches('\u{FE0F}');
    if let Some(p) = idx.get(stripped) {
        return Some(*p);
    }
    let with_vs = format!("{stripped}\u{FE0F}");
    idx.get(with_vs.as_str()).copied()
}

/// Append the wire marker for `effect_id` to an outgoing body. No-op for 0 or an
/// unknown id. The marker sits at the very end so stripping is a cheap suffix
/// check on receipt.
pub(crate) fn append_effect_marker(text: &str, effect_id: i32) -> String {
    match effect_key(effect_id) {
        Some(key) => format!("{text}{FX_MARK}dmfx:{key}{FX_MARK}"),
        None => text.to_string(),
    }
}

/// Split a trailing effect marker off a raw body, returning (clean_body,
/// effect_id). Returns the input untouched (effect 0) when there's no valid
/// marker, so it's safe to run on every body unconditionally.
pub(crate) fn split_effect_marker(raw: &str) -> (&str, i32) {
    let m = FX_MARK.len_utf8();
    if !raw.ends_with(FX_MARK) {
        return (raw, 0);
    }
    let head = &raw[..raw.len() - m];
    let Some(pos) = head.rfind(FX_MARK) else {
        return (raw, 0);
    };
    let inner = &head[pos + m..];
    let Some(key) = inner.strip_prefix("dmfx:") else {
        return (raw, 0);
    };
    let id = effect_id_from_key(key);
    if id == 0 {
        return (raw, 0);
    }
    (&head[..pos], id)
}

/// Set of message-ids whose effect has already been claimed for autoplay (or
/// marked seen-during-backfill). Rows rebuild from scratch (reactions, picture
/// loads, full rebuilds recreate components and re-run `init`), so the
/// fire-exactly-once decision can't live in Slint state — it lives here.
pub(crate) fn effect_seen_ids() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    use std::sync::OnceLock;
    static S: OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    S.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Whether this build should AUTOPLAY the effect for `message_id`. True only on
/// the very first time the id is ever built, and only if that first build is
/// `live` (a watcher arrival or optimistic send). Every later build — including
/// a full rebuild that recreates the row component and re-runs its `init`, or a
/// chat reopen — returns false, so a playing burst is never re-fired or
/// interrupted. Backfill (`live == false`) just claims the id as seen-but-quiet.
/// (Tap-to-replay is independent of this — it fires straight from the bubble.)
pub(crate) fn effect_should_autoplay(message_id: &str, raw_effect: i32, live: bool) -> bool {
    if raw_effect == 0 {
        return false;
    }
    let mut seen = effect_seen_ids().lock().unwrap();
    if !seen.insert(message_id.to_string()) {
        // Already present → already handled once; never autoplay again.
        return false;
    }
    live
}

// ── Markdown rendering ──────────────────────────────────────────────────────
//
// Chat bodies are parsed with `whitenoise_markdown` (the same CommonMark + GFM +
// nostr parser whitenoise-rs uses) into a `Document`, then flattened into the
// bubble's existing line/run model: each `MessageLine` is one visual line, each
// `MessageRun` an inline text/emoji cell carrying resolved styling. Block
// context (heading scale, list/blockquote indent, code plates, rules) rides on
// the line. Wrapping stays Rust-side and greedy — widths are estimated against
// the monospace advance only to decide *where* to break; Slint draws with real
// metrics, exactly as the plain-text path did before.
use whitenoise_markdown::{Block, Inline, ListItem, ListKind, NostrEntity};

/// Approximate monospace glyph advance as a fraction of font-size. Only used to
/// decide wrap points; never to position glyphs.
pub(crate) const MD_CHAR_W: f32 = 0.62;
/// Approximate inline-emoji advance as a fraction of font-size.
pub(crate) const MD_EMOJI_W: f32 = 1.25;

/// Inline styling resolved for a single run while walking the AST.
#[derive(Clone, Copy, Default)]
pub(crate) struct MdStyle {
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
    /// iMessage text effects as a bitmask, so any number of effects stack on
    /// the same glyph (each acts on an independent visual axis). Bits:
    /// 1 big, 2 small, 4 explode, 8 bloom, 16 shake, 32 nod, 64 ripple,
    /// 128 jitter. The RunCell decodes the bits and composes the transforms.
    fx: u8,
}

/// OR the `{name}…{/name}` effect into the style's bitmask. Effects compose, so
/// nesting (`{big}{explode}…`) keeps both. Unknown names set no bit, so the
/// span passes through as literal styled text.
pub(crate) fn apply_effect(style: &mut MdStyle, name: &str) {
    let bit: u8 = match name.to_ascii_lowercase().as_str() {
        "big" => 1,
        "small" => 2,
        "explode" => 4,
        "bloom" => 8,
        "shake" => 16,
        "nod" => 32,
        "ripple" => 64,
        "jitter" => 128,
        _ => 0,
    };
    style.fx |= bit;
}

impl MdStyle {
    /// True when any effect bit is set — drives per-letter splitting.
    fn has_fx(&self) -> bool {
        self.fx != 0
    }
}

/// One atomic token in the inline stream feeding the greedy wrapper.
pub(crate) enum MdTok {
    Word {
        text: String,
        style: MdStyle,
        link: Option<String>,
    },
    Space {
        text: String,
        style: MdStyle,
        link: Option<String>,
    },
    Emoji {
        x: u32,
        y: u32,
        fx: u8,
    },
    Break,
}

/// A wrapped line plus its block-level context, before conversion to the Slint
/// `MessageLine` struct.
pub(crate) struct MdLine {
    runs: Vec<MessageRun>,
    indent: f32,
    scale: f32,
    quote: i32,
    code_block: bool,
    rule: bool,
}

/// Block-walk context: accumulated left inset and current blockquote depth.
#[derive(Clone, Copy)]
pub(crate) struct MdCtx {
    indent: f32,
    quote: i32,
}

/// Render-wide constants threaded through the whole block walk: the wrap width,
/// the base font size, and the emoji sprite-position index. Bundled (and `Copy`,
/// since `positions` is `&'static`) so the recursive walkers pass a single `env`
/// instead of repeating the same three positional arguments at every call.
#[derive(Clone, Copy)]
pub(crate) struct MdEnv {
    max_width: f32,
    base_fs: f32,
    positions: &'static std::collections::HashMap<&'static str, (u32, u32)>,
}

pub(crate) fn md_run_text(text: &str, style: MdStyle, link: &Option<String>) -> MessageRun {
    MessageRun {
        is_emoji: false,
        text: SharedString::from(text),
        clip_x: 0,
        clip_y: 0,
        bold: style.bold,
        italic: style.italic,
        strike: style.strike,
        code: style.code,
        link: link.as_deref().map(SharedString::from).unwrap_or_default(),
        fx: style.fx as i32,
        // Assigned in a second pass over the finished runs (md_assign_phases).
        phase: 0.0,
    }
}

pub(crate) fn md_run_emoji(x: u32, y: u32, fx: u8) -> MessageRun {
    MessageRun {
        is_emoji: true,
        text: SharedString::new(),
        clip_x: x as i32,
        clip_y: y as i32,
        bold: false,
        italic: false,
        strike: false,
        code: false,
        link: SharedString::new(),
        fx: fx as i32,
        phase: 0.0,
    }
}

/// Shorten a long bech32 (or any) string to `head…tail` for ergonomic display.
pub(crate) fn md_shorten(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= 18 {
        return s.to_string();
    }
    let head: String = chars[..12].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// Tokenize a raw text run into words / whitespace / (optionally) emoji,
/// tagging each with the active style + link. Emoji probing is skipped for
/// code spans, where glyph sequences must stay literal.
pub(crate) fn md_push_text(
    out: &mut Vec<MdTok>,
    text: &str,
    style: MdStyle,
    link: &Option<String>,
    positions: &std::collections::HashMap<&'static str, (u32, u32)>,
    probe_emoji: bool,
) {
    let mut buf = String::new();
    let mut buf_space = false;
    let flush = |buf: &mut String, is_space: &mut bool, out: &mut Vec<MdTok>| {
        if buf.is_empty() {
            return;
        }
        let taken = std::mem::take(buf);
        if *is_space {
            out.push(MdTok::Space {
                text: taken,
                style,
                link: link.clone(),
            });
        } else {
            out.push(MdTok::Word {
                text: taken,
                style,
                link: link.clone(),
            });
        }
        *is_space = false;
    };

    let mut i = 0;
    while i < text.len() {
        if probe_emoji {
            // Probe for the longest emoji match at `i`. ZWJ sequences can be
            // ~30+ bytes; 48 is a comfortable cap.
            let end_max = (i + 48).min(text.len());
            let mut matched: Option<(usize, u32, u32)> = None;
            for end in (i + 1..=end_max).rev() {
                if !text.is_char_boundary(end) {
                    continue;
                }
                if let Some(&(x, y)) = positions.get(&text[i..end]) {
                    matched = Some((end, x, y));
                    break;
                }
            }
            if let Some((end, x, y)) = matched {
                flush(&mut buf, &mut buf_space, out);
                out.push(MdTok::Emoji { x, y, fx: style.fx });
                i = end;
                continue;
            }
        }
        let c = text[i..].chars().next().unwrap();
        let clen = c.len_utf8();
        if c.is_whitespace() {
            if !buf_space {
                flush(&mut buf, &mut buf_space, out);
                buf_space = true;
            }
            buf.push(c);
        } else {
            if buf_space {
                flush(&mut buf, &mut buf_space, out);
            }
            buf.push(c);
            // Effect runs render per-letter so motion effects (ripple, jitter,
            // shake…) animate each glyph independently, like iMessage. Flush
            // after every visible character so each becomes its own run/cell.
            if style.has_fx() {
                flush(&mut buf, &mut buf_space, out);
            }
        }
        i += clen;
    }
    flush(&mut buf, &mut buf_space, out);
}

/// Emit a nostr entity as a single shortened, linkified word.
pub(crate) fn md_push_nostr(out: &mut Vec<MdTok>, e: &NostrEntity, style: MdStyle, mention: bool) {
    let mut display = md_shorten(&e.bech32);
    if mention {
        display = format!("@{display}");
    }
    out.push(MdTok::Word {
        text: display,
        style,
        link: Some(format!("nostr:{}", e.bech32)),
    });
}

/// Recursively flatten inline AST nodes into styled tokens.
pub(crate) fn md_walk_inlines(
    out: &mut Vec<MdTok>,
    inlines: &[Inline],
    style: MdStyle,
    link: Option<String>,
    positions: &std::collections::HashMap<&'static str, (u32, u32)>,
) {
    for inl in inlines {
        match inl {
            Inline::Text(s) => md_push_text(out, s, style, &link, positions, true),
            Inline::SoftBreak | Inline::HardBreak => out.push(MdTok::Break),
            Inline::Code(s) => {
                let st = MdStyle {
                    code: true,
                    ..style
                };
                md_push_text(out, s, st, &link, positions, false);
            }
            Inline::Emph(c) => md_walk_inlines(
                out,
                c,
                MdStyle {
                    italic: true,
                    ..style
                },
                link.clone(),
                positions,
            ),
            Inline::Strong(c) => md_walk_inlines(
                out,
                c,
                MdStyle {
                    bold: true,
                    ..style
                },
                link.clone(),
                positions,
            ),
            Inline::Strikethrough(c) => md_walk_inlines(
                out,
                c,
                MdStyle {
                    strike: true,
                    ..style
                },
                link.clone(),
                positions,
            ),
            Inline::Link { dest, children, .. } => {
                md_walk_inlines(out, children, style, Some(dest.clone()), positions)
            }
            Inline::Image { dest, alt, .. } => {
                let l = Some(dest.clone());
                md_push_text(out, "🖼", style, &l, positions, true);
                md_push_text(out, " ", style, &l, positions, false);
                if alt.is_empty() {
                    md_push_text(out, dest, style, &l, positions, false);
                } else {
                    md_walk_inlines(out, alt, style, l, positions);
                }
            }
            Inline::Autolink { url, .. } => {
                md_push_text(out, url, style, &Some(url.clone()), positions, false)
            }
            Inline::Math(s) => {
                let st = MdStyle {
                    code: true,
                    ..style
                };
                md_push_text(out, s, st, &link, positions, false);
            }
            Inline::NostrMention(e) => md_push_nostr(out, e, style, true),
            Inline::NostrUri(e) => md_push_nostr(out, e, style, false),
            Inline::Effect { name, children } => {
                // Set the matching channel (size or motion) on top of the
                // inherited style, so nesting stacks instead of overwriting.
                // Unknown names leave both channels untouched → pass-through.
                let mut st = style;
                apply_effect(&mut st, name);
                md_walk_inlines(out, children, st, link.clone(), positions);
            }
        }
    }
}

/// Font scale for an ATX heading level.
pub(crate) fn md_heading_scale(level: u8) -> f32 {
    match level {
        1 => 1.5,
        2 => 1.34,
        3 => 1.2,
        4 => 1.1,
        _ => 1.04,
    }
}

/// A thin blank line used to separate sibling blocks.
pub(crate) fn md_spacer(ctx: MdCtx) -> MdLine {
    MdLine {
        runs: vec![md_run_text(" ", MdStyle::default(), &None)],
        indent: ctx.indent,
        scale: 0.4,
        quote: ctx.quote,
        code_block: false,
        rule: false,
    }
}

/// Greedy-pack a token stream into wrapped lines under `max_width` (minus the
/// block indent). Over-long single tokens (URLs, code) are hard-split so they
/// never overflow the bubble.
pub(crate) fn md_wrap(
    out: &mut Vec<MdLine>,
    toks: Vec<MdTok>,
    env: MdEnv,
    indent: f32,
    scale: f32,
    quote: i32,
    code_block: bool,
) {
    let char_w = env.base_fs * MD_CHAR_W * scale;
    let emoji_w = env.base_fs * MD_EMOJI_W * scale;
    let avail = (env.max_width - indent).max(40.0);
    let max_chars = ((avail / char_w).floor() as usize).max(1);

    let mut cur: Vec<MessageRun> = Vec::new();
    let mut x = 0.0f32;
    let flush = |out: &mut Vec<MdLine>, cur: &mut Vec<MessageRun>| {
        out.push(MdLine {
            runs: std::mem::take(cur),
            indent,
            scale,
            quote,
            code_block,
            rule: false,
        });
    };

    for tok in toks {
        match tok {
            MdTok::Break => {
                flush(out, &mut cur);
                x = 0.0;
            }
            MdTok::Space { text, style, link } => {
                // Drop whitespace at the start of a wrapped line — except in
                // code blocks, where leading indentation is significant.
                if x == 0.0 && !code_block {
                    continue;
                }
                x += text.chars().count() as f32 * char_w;
                cur.push(md_run_text(&text, style, &link));
            }
            MdTok::Emoji { x: ex, y: ey, fx } => {
                if x > 0.0 && x + emoji_w > avail {
                    flush(out, &mut cur);
                    x = 0.0;
                }
                cur.push(md_run_emoji(ex, ey, fx));
                x += emoji_w;
            }
            MdTok::Word { text, style, link } => {
                let n = text.chars().count();
                let w = n as f32 * char_w;
                if w <= avail {
                    if x > 0.0 && x + w > avail {
                        flush(out, &mut cur);
                        x = 0.0;
                    }
                    cur.push(md_run_text(&text, style, &link));
                    x += w;
                } else {
                    // Hard-split an over-long token into width-fitting chunks.
                    let chars: Vec<char> = text.chars().collect();
                    let mut start = 0;
                    while start < chars.len() {
                        if x > 0.0 {
                            flush(out, &mut cur);
                            x = 0.0;
                        }
                        let end = (start + max_chars).min(chars.len());
                        let chunk: String = chars[start..end].iter().collect();
                        cur.push(md_run_text(&chunk, style, &link));
                        x += (end - start) as f32 * char_w;
                        start = end;
                    }
                }
            }
        }
    }
    if !cur.is_empty() {
        flush(out, &mut cur);
    }
}

/// Render one table row as a wrapped line, cells separated by a thin divider.
pub(crate) fn md_emit_table_row(
    out: &mut Vec<MdLine>,
    cells: &[whitenoise_markdown::TableCell],
    header: bool,
    ctx: MdCtx,
    env: MdEnv,
) {
    let mut toks = Vec::new();
    for (ci, cell) in cells.iter().enumerate() {
        if ci > 0 {
            toks.push(MdTok::Word {
                text: "│".to_string(),
                style: MdStyle::default(),
                link: None,
            });
            toks.push(MdTok::Space {
                text: " ".to_string(),
                style: MdStyle::default(),
                link: None,
            });
        }
        md_walk_inlines(
            &mut toks,
            &cell.inlines,
            MdStyle {
                bold: header,
                ..Default::default()
            },
            None,
            env.positions,
        );
        toks.push(MdTok::Space {
            text: " ".to_string(),
            style: MdStyle::default(),
            link: None,
        });
    }
    md_wrap(out, toks, env, ctx.indent, 1.0, ctx.quote, false);
}

/// Render the items of a list, placing each item's marker on its first line and
/// indenting wrapped / nested content under it.
pub(crate) fn md_walk_list(
    out: &mut Vec<MdLine>,
    kind: ListKind,
    tight: bool,
    items: &[ListItem],
    ctx: MdCtx,
    env: MdEnv,
) {
    let mut number = match kind {
        ListKind::Ordered { start, .. } => start,
        ListKind::Bullet { .. } => 0,
    };
    for (ii, item) in items.iter().enumerate() {
        if ii > 0 && !tight {
            out.push(md_spacer(ctx));
        }
        let mut marker = match kind {
            ListKind::Ordered { .. } => {
                let m = format!("{number}. ");
                number += 1;
                m
            }
            ListKind::Bullet { .. } => "•  ".to_string(),
        };
        if let Some(checked) = item.checked {
            marker.push_str(if checked { "☑ " } else { "☐ " });
        }
        let marker_w = marker.chars().count() as f32 * env.base_fs * MD_CHAR_W;
        let child = MdCtx {
            indent: ctx.indent + marker_w,
            quote: ctx.quote,
        };
        let mut tmp: Vec<MdLine> = Vec::new();
        md_walk_blocks(&mut tmp, &item.blocks, child, env);
        if tmp.is_empty() {
            tmp.push(MdLine {
                runs: Vec::new(),
                indent: child.indent,
                scale: 1.0,
                quote: ctx.quote,
                code_block: false,
                rule: false,
            });
        }
        // The marker sits at the item's own indent; content trails after it.
        tmp[0].indent = ctx.indent;
        tmp[0]
            .runs
            .insert(0, md_run_text(&marker, MdStyle::default(), &None));
        out.extend(tmp);
    }
}

/// Recursively flatten block AST nodes into wrapped, context-tagged lines.
pub(crate) fn md_walk_blocks(out: &mut Vec<MdLine>, blocks: &[Block], ctx: MdCtx, env: MdEnv) {
    for (bi, b) in blocks.iter().enumerate() {
        if bi > 0 {
            out.push(md_spacer(ctx));
        }
        match b {
            Block::Paragraph { inlines } => {
                let mut toks = Vec::new();
                md_walk_inlines(&mut toks, inlines, MdStyle::default(), None, env.positions);
                md_wrap(out, toks, env, ctx.indent, 1.0, ctx.quote, false);
            }
            Block::Heading { level, inlines } => {
                let mut toks = Vec::new();
                md_walk_inlines(
                    &mut toks,
                    inlines,
                    MdStyle {
                        bold: true,
                        ..Default::default()
                    },
                    None,
                    env.positions,
                );
                md_wrap(
                    out,
                    toks,
                    env,
                    ctx.indent,
                    md_heading_scale(*level),
                    ctx.quote,
                    false,
                );
            }
            Block::ThematicBreak => out.push(MdLine {
                runs: Vec::new(),
                indent: ctx.indent,
                scale: 1.0,
                quote: ctx.quote,
                code_block: false,
                rule: true,
            }),
            Block::CodeBlock { content, .. } => {
                let body = content.strip_suffix('\n').unwrap_or(content);
                let st = MdStyle {
                    code: true,
                    ..Default::default()
                };
                for line in body.split('\n') {
                    if line.is_empty() {
                        out.push(MdLine {
                            runs: vec![md_run_text(" ", st, &None)],
                            indent: ctx.indent,
                            scale: 1.0,
                            quote: ctx.quote,
                            code_block: true,
                            rule: false,
                        });
                        continue;
                    }
                    let mut toks = Vec::new();
                    md_push_text(&mut toks, line, st, &None, env.positions, false);
                    md_wrap(out, toks, env, ctx.indent, 1.0, ctx.quote, true);
                }
            }
            Block::BlockQuote { blocks } => {
                let inner = MdCtx {
                    indent: ctx.indent + 12.0,
                    quote: ctx.quote + 1,
                };
                md_walk_blocks(out, blocks, inner, env);
            }
            Block::List { kind, tight, items } => {
                md_walk_list(out, *kind, *tight, items, ctx, env);
            }
            Block::Table { header, rows, .. } => {
                md_emit_table_row(out, header, true, ctx, env);
                for row in rows {
                    md_emit_table_row(out, row, false, ctx, env);
                }
            }
            Block::MathBlock { content } => {
                let body = content.strip_suffix('\n').unwrap_or(content);
                let st = MdStyle {
                    code: true,
                    ..Default::default()
                };
                for line in body.split('\n') {
                    let mut toks = Vec::new();
                    md_push_text(&mut toks, line, st, &None, env.positions, false);
                    md_wrap(out, toks, env, ctx.indent, 1.0, ctx.quote, true);
                }
            }
        }
    }
}

/// Parse a chat-message body as Markdown and flatten it into pre-wrapped lines.
/// Second pass over finished runs: stagger each effect run's `phase` so motion
/// effects animate per-letter rather than in lockstep. The counter advances
/// once per effect cell (giving Ripple its travelling crest and Jitter its
/// decorrelated wobble) and resets on any non-effect run so each contiguous
/// span starts its wave fresh.
pub(crate) fn md_assign_phases(lines: &mut [MdLine]) {
    let mut step: u32 = 0;
    for line in lines.iter_mut() {
        for run in line.runs.iter_mut() {
            if run.fx != 0 {
                run.phase = step as f32 * 0.12;
                step += 1;
            } else {
                step = 0;
            }
        }
    }
}

pub(crate) fn tokenize_message_lines(text: &str, max_width: f32, base_fs: f32) -> Vec<MessageLine> {
    let positions = emoji_position_index();
    let doc = whitenoise_markdown::parse(text);
    let mut lines: Vec<MdLine> = Vec::new();
    let env = MdEnv {
        max_width,
        base_fs,
        positions,
    };
    md_walk_blocks(
        &mut lines,
        &doc.blocks,
        MdCtx {
            indent: 0.0,
            quote: 0,
        },
        env,
    );
    md_assign_phases(&mut lines);
    lines
        .into_iter()
        .map(|l| MessageLine {
            runs: ModelRc::new(VecModel::from(l.runs)),
            indent: l.indent,
            scale: l.scale,
            quote: l.quote,
            code_block: l.code_block,
            rule: l.rule,
        })
        .collect()
}

/// Open a URL (or `mailto:` / `nostr:` URI) with the platform's default
/// handler. Fire-and-forget; failures are logged, not surfaced.
pub(crate) fn open_external(url: &str) {
    if url.is_empty() {
        return;
    }
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";
    match std::process::Command::new(program).arg(url).spawn() {
        Ok(_) => tracing::debug!(url, "opened external link"),
        Err(e) => tracing::warn!(url, error = %e, "failed to open external link"),
    }
}

/// Find an active `@mention` token ending at the caret. Returns the byte offset
/// of the `@` and the query text between it and the caret. The token is active
/// only when the `@` sits at the start or directly after whitespace, and there
/// is no whitespace between it and the caret.
pub(crate) fn detect_mention(text: &str, cursor: usize) -> Option<(usize, String)> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let prefix = &text[..cursor];
    let mut at_byte = None;
    for (i, c) in prefix.char_indices().rev() {
        if c == '@' {
            let preceded_ok = i == 0
                || prefix[..i]
                    .chars()
                    .next_back()
                    .map(|p| p.is_whitespace())
                    .unwrap_or(true);
            if preceded_ok {
                at_byte = Some(i);
            }
            break;
        }
        if c.is_whitespace() {
            break;
        }
    }
    let at = at_byte?;
    Some((at, prefix[at + 1..].to_string()))
}

/// Filter the open chat's members by a mention query (matches display name,
/// short npub, or full npub; an empty query lists everyone). Capped at 50.
pub(crate) fn filter_mention_candidates(ui: &DarkMatterLinux, query: &str) -> Vec<GroupMember> {
    let q = query.to_lowercase();
    ui.get_chat_members()
        .iter()
        .filter(|m| {
            if q.is_empty() {
                return true;
            }
            if m.name.as_str().to_lowercase().contains(&q)
                || m.npub_short.as_str().to_lowercase().contains(&q)
            {
                return true;
            }
            npub_for_account_id(m.member_id.as_str())
                .map(|n| n.to_lowercase().contains(&q))
                .unwrap_or(false)
        })
        .take(50)
        .collect()
}

/// Splice the chosen member's npub over the active `@token` and place the caret
/// just after the inserted mention.
pub(crate) fn commit_mention(
    ui: &DarkMatterLinux,
    mention_span: &Rc<RefCell<Option<(usize, usize)>>>,
    index: i32,
) {
    ui.set_mention_active(false);
    let Some((at, end)) = mention_span.borrow_mut().take() else {
        return;
    };
    if index < 0 {
        return;
    }
    let cands = ui.get_mention_candidates();
    let Some(member) = cands.row_data(index as usize) else {
        return;
    };
    let npub = npub_for_account_id(member.member_id.as_str())
        .unwrap_or_else(|_| member.member_id.to_string());
    let mut draft = ui.get_composer_draft().to_string();
    if at > end || end > draft.len() || !draft.is_char_boundary(at) || !draft.is_char_boundary(end)
    {
        return;
    }
    let insert = format!("@{npub} ");
    let new_cursor = at + insert.len();
    draft.replace_range(at..end, &insert);
    ui.set_composer_draft(draft.into());
    // Move the caret past the inserted mention (tick forces re-apply).
    ui.set_composer_caret_pos(new_cursor as i32);
    ui.set_composer_caret_tick(ui.get_composer_caret_tick().wrapping_add(1));
}

// Memoized markdown line models, keyed by (body, wrap-width). Rebuilding a
// chat re-parses every visible body through the full markdown → wrap pipeline;
// bodies are immutable (edits arrive as new text → new key), so the flattened
// model can be shared across rows and rebuilds. UI-thread only (the line
// models hold `ModelRc`s, which are not `Send`). Bounded: wholesale-cleared
// at the cap rather than LRU-tracked — a full re-parse of one chat is cheap
// compared to bookkeeping on every hit.
thread_local! {
    static MESSAGE_LINES_CACHE: RefCell<HashMap<(String, u32), ModelRc<MessageLine>>> =
        RefCell::new(HashMap::new());
}
pub(crate) const MESSAGE_LINES_CACHE_CAP: usize = 4096;

/// Build the `lines` model for `ChatMessage` from the message body.
pub(crate) fn build_message_lines(text: &str, bubble_max: f32) -> ModelRc<MessageLine> {
    // Chat-body chrome: 2*pad-h (14) + gap (12) + meta col (~70). Conservative
    // so wrapping kicks in before the dynamic `available-w` clips the bubble.
    let budget = (bubble_max - 110.0).max(60.0);
    MESSAGE_LINES_CACHE.with(|cache| {
        let key = (text.to_string(), budget.to_bits());
        if let Some(model) = cache.borrow().get(&key) {
            return model.clone();
        }
        let model: ModelRc<MessageLine> =
            ModelRc::new(VecModel::from(tokenize_message_lines(text, budget, 13.0)));
        let mut cache = cache.borrow_mut();
        if cache.len() >= MESSAGE_LINES_CACHE_CAP {
            cache.clear();
        }
        cache.insert(key, model.clone());
        model
    })
}

/// Telegram-style jumbo-emoji test. If `text` is nothing but emoji (plus
/// whitespace) and short enough — at most [`JUMBO_EMOJI_MAX`] glyphs — return
/// the emoji count; otherwise 0. The probe mirrors the tokenizer's longest-
/// match-against-the-sprite-table logic ([`md_push_text`]) so what we classify
/// as jumbo is exactly what would render as sprite cells.
pub(crate) const JUMBO_EMOJI_MAX: u32 = 3;
pub(crate) fn jumbo_emoji_count(text: &str) -> u32 {
    let positions = emoji_position_index();
    let t = text.trim();
    if t.is_empty() {
        return 0;
    }
    let mut count = 0u32;
    let mut i = 0usize;
    while i < t.len() {
        let c = t[i..].chars().next().unwrap();
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        // Longest emoji match at `i` (ZWJ sequences run ~30+ bytes; 48 caps it).
        let end_max = (i + 48).min(t.len());
        let mut matched = None;
        for end in (i + 1..=end_max).rev() {
            if t.is_char_boundary(end) && positions.contains_key(&t[i..end]) {
                matched = Some(end);
                break;
            }
        }
        match matched {
            Some(end) => {
                count += 1;
                if count > JUMBO_EMOJI_MAX {
                    return 0;
                }
                i = end;
            }
            // Any non-emoji glyph disqualifies the whole message.
            None => return 0,
        }
    }
    count
}

/// Filter the full emoji catalog by `query` (matches name + shortcodes,
/// lowercased substring) and return the flat list. The Slint side handles
/// column packing and virtualization.
pub(crate) fn build_emoji_list(query: &str) -> Vec<EmojiEntry> {
    let q = query.trim().to_lowercase();
    let positions = emoji_position_index();
    let mut hits: Vec<EmojiEntry> = Vec::new();
    for e in emojis::iter() {
        if !q.is_empty() {
            let name_match = e.name().to_lowercase().contains(&q);
            let code_match = e.shortcodes().any(|c| c.to_lowercase().contains(&q));
            if !name_match && !code_match {
                continue;
            }
        }
        // Skip emojis the build-time sprite sheet doesn't cover.
        let Some(&(x, y)) = positions.get(e.as_str()) else {
            continue;
        };
        hits.push(EmojiEntry {
            emoji: s(e.as_str()),
            name: s(e.name()),
            clip_x: x as i32,
            clip_y: y as i32,
        });
    }
    hits
}

/// Run [`copy_to_clipboard`] on a throwaway worker thread and hand the result
/// back on the UI thread. The CLI helpers and arboard can all wait on the
/// display server (wedged compositor, full pipe buffer, slow X11 connect), so
/// the UI thread must never call [`copy_to_clipboard`] directly.
pub(crate) fn copy_to_clipboard_async(
    text: String,
    on_done: impl FnOnce(Result<(), String>) + Send + 'static,
) {
    std::thread::spawn(move || {
        let result = copy_to_clipboard(&text);
        let _ = slint::invoke_from_event_loop(move || on_done(result));
    });
}

/// Push `text` to the system clipboard. Blocking — see
/// [`copy_to_clipboard_async`] for the only form UI callbacks may use.
///
/// On Linux/FreeBSD, arboard's Wayland support is finicky — it depends on
/// the compositor exposing the right data-control protocol, and on some
/// stacks `set_text` returns `Ok` without anyone actually getting the
/// content. Instead, we prefer the standard CLI tools that ship in every
/// desktop install: `wl-copy` for Wayland, `xclip`/`xsel` for X11. On macOS
/// the native `pbcopy` helper is always present (and the Wayland/X11 tools
/// would talk to an XQuartz clipboard, not the system one, so they are
/// skipped entirely). arboard stays as a final fallback everywhere.
pub(crate) fn copy_to_clipboard(text: &str) -> Result<(), String> {
    let preview: String = text.chars().take(24).collect();
    eprintln!(
        "[clipboard] copy len={} preview={:?}{} WAYLAND_DISPLAY={:?} DISPLAY={:?}",
        text.len(),
        preview,
        if text.len() > 24 { "…" } else { "" },
        std::env::var_os("WAYLAND_DISPLAY"),
        std::env::var_os("DISPLAY"),
    );

    #[cfg(target_os = "macos")]
    {
        match copy_via_command("pbcopy", &[], text) {
            Ok(()) => {
                eprintln!("[clipboard] via pbcopy ok");
                return Ok(());
            }
            Err(e) => eprintln!("[clipboard] pbcopy failed: {e}"),
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        // Wayland session?
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            match copy_via_command("wl-copy", &[], text) {
                Ok(()) => {
                    eprintln!("[clipboard] via wl-copy ok");
                    return Ok(());
                }
                Err(e) => eprintln!("[clipboard] wl-copy failed: {e}"),
            }
        }

        // X11 session?
        if std::env::var_os("DISPLAY").is_some() {
            for (cmd, args) in [
                ("xclip", &["-selection", "clipboard"][..]),
                ("xsel", &["--clipboard", "--input"][..]),
            ] {
                match copy_via_command(cmd, args, text) {
                    Ok(()) => {
                        eprintln!("[clipboard] via {cmd} ok");
                        return Ok(());
                    }
                    Err(e) => eprintln!("[clipboard] {cmd} failed: {e}"),
                }
            }
        }
    }

    // Last resort: arboard. Hold a single long-lived Clipboard so we don't
    // immediately drop ownership.
    use std::sync::{Mutex, OnceLock};
    static CLIPBOARD: OnceLock<Mutex<arboard::Clipboard>> = OnceLock::new();
    let cb = CLIPBOARD.get_or_init(|| {
        Mutex::new(arboard::Clipboard::new().expect("clipboard backend init failed"))
    });
    let mut guard = cb.lock().map_err(|e| e.to_string())?;
    match guard.set_text(text.to_string()) {
        Ok(()) => {
            eprintln!("[clipboard] via arboard ok");
            Ok(())
        }
        Err(e) => {
            eprintln!("[clipboard] arboard failed: {e}");
            Err(e.to_string())
        }
    }
}

/// Spawn a CLI clipboard helper, pipe `text` into its stdin, wait for the
/// parent to exit (these helpers fork themselves into the background after
/// reading stdin, so the parent exits in milliseconds), and surface the
/// exit code if anything went wrong.
///
/// stdout/stderr must NOT be `Stdio::piped()`: the forked background child
/// that keeps serving the clipboard inherits the pipe write ends, so reading
/// them to EOF (e.g. `wait_with_output`) blocks until clipboard ownership is
/// lost — which freezes the UI thread. stderr is inherited instead, so any
/// helper diagnostics still land in our own stderr log.
pub(crate) fn copy_via_command(cmd: &str, args: &[&str], text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    eprintln!("[clipboard] spawning: {cmd} {args:?}");
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| format!("write stdin: {e}"))?;
        // dropping stdin closes the pipe so the helper sees EOF
    }
    let status = child.wait().map_err(|e| format!("wait: {e}"))?;
    if !status.success() {
        return Err(format!("{cmd} exited {status} (stderr passed through)"));
    }
    Ok(())
}

/// Choose an image MIME target from a clipboard target list, applying the
/// image-intent rule: any plain-text target wins (returns `None` — the
/// native text paste already handled it, and sources that offer *both*
/// mean text), otherwise `image/png` is preferred over whatever other
/// `image/*` target comes first.
pub(crate) fn pick_image_target(types: &[&str]) -> Option<String> {
    let has_text = types
        .iter()
        .any(|t| *t == "UTF8_STRING" || *t == "STRING" || t.starts_with("text/plain"));
    if has_text {
        return None;
    }
    if types.contains(&"image/png") {
        return Some("image/png".to_string());
    }
    types
        .iter()
        .find(|t| t.starts_with("image/"))
        .map(|t| t.to_string())
}

/// Read image bytes off the system clipboard, but only when the clipboard
/// looks image-intent (see [`pick_image_target`]). Mirrors
/// [`copy_to_clipboard`]'s platform ladder: `wl-paste` on Wayland,
/// `xclip` on X11, arboard as the fallback everywhere and the primary
/// path on macOS (`pbpaste` is text-only). Blocking — subprocess /
/// display-server round-trips — so never call on the UI thread. Returns
/// `(bytes, media_type)`.
pub(crate) fn paste_image_from_clipboard() -> Option<(Vec<u8>, String)> {
    #[cfg(not(target_os = "macos"))]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            match paste_via_command("wl-paste", &["--list-types"]) {
                Ok(out) => {
                    let listing = String::from_utf8_lossy(&out).into_owned();
                    let types: Vec<&str> = listing.lines().map(str::trim).collect();
                    // wl-paste answered: it owns the truth about this
                    // clipboard, so no image target (or text intent) is a
                    // final no — don't fall through to arboard.
                    let mime = pick_image_target(&types)?;
                    return match paste_via_command("wl-paste", &["--no-newline", "--type", &mime]) {
                        Ok(bytes) if !bytes.is_empty() => {
                            eprintln!(
                                "[clipboard] image via wl-paste ({mime}, {} bytes)",
                                bytes.len()
                            );
                            Some((bytes, mime))
                        }
                        Ok(_) => None,
                        Err(e) => {
                            eprintln!("[clipboard] wl-paste read failed: {e}");
                            None
                        }
                    };
                }
                Err(e) => eprintln!("[clipboard] wl-paste list-types failed: {e}"),
            }
        }
        if std::env::var_os("DISPLAY").is_some() {
            match paste_via_command("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"]) {
                Ok(out) => {
                    let listing = String::from_utf8_lossy(&out).into_owned();
                    let types: Vec<&str> = listing.lines().map(str::trim).collect();
                    let mime = pick_image_target(&types)?;
                    return match paste_via_command(
                        "xclip",
                        &["-selection", "clipboard", "-t", &mime, "-o"],
                    ) {
                        Ok(bytes) if !bytes.is_empty() => {
                            eprintln!(
                                "[clipboard] image via xclip ({mime}, {} bytes)",
                                bytes.len()
                            );
                            Some((bytes, mime))
                        }
                        Ok(_) => None,
                        Err(e) => {
                            eprintln!("[clipboard] xclip read failed: {e}");
                            None
                        }
                    };
                }
                Err(e) => eprintln!("[clipboard] xclip targets failed: {e}"),
            }
        }
    }

    // arboard fallback. It hands back raw RGBA, so re-encode as PNG for the
    // upload path (which wants original compressed bytes).
    let mut cb = match arboard::Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[clipboard] arboard init failed: {e}");
            return None;
        }
    };
    if matches!(cb.get_text(), Ok(t) if !t.is_empty()) {
        return None; // text intent — native paste already handled it
    }
    let img = cb.get_image().ok()?;
    let (w, h) = (img.width as u32, img.height as u32);
    let rgba = image::RgbaImage::from_raw(w, h, img.bytes.into_owned())?;
    let mut png = Vec::new();
    if let Err(e) = image::DynamicImage::ImageRgba8(rgba)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
    {
        eprintln!("[clipboard] png encode failed: {e}");
        return None;
    }
    eprintln!("[clipboard] image via arboard ({w}x{h})");
    Some((png, "image/png".to_string()))
}

/// Run a CLI clipboard *reader* and capture its stdout bytes. Unlike the
/// copy helpers these don't fork into the background, so a plain
/// `output()` is safe.
pub(crate) fn paste_via_command(cmd: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    use std::process::{Command, Stdio};
    let out = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!("{cmd} exited {}", out.status));
    }
    Ok(out.stdout)
}

// ─── Keys & key packages ───────────────────────────────────────────────

pub(crate) fn kp_to_ui(rec: &marmot_app::AccountKeyPackageRecord) -> KeyPackageInfo {
    let short_ref: String = rec.key_package_ref_hex.chars().take(16).collect();
    let short_ref = if rec.key_package_ref_hex.len() > 16 {
        format!("{short_ref}…")
    } else {
        short_ref
    };
    KeyPackageInfo {
        key_package_id: s(&rec.key_package_id),
        key_package_ref: s(&short_ref),
        event_id: s(&rec.key_package_event_id),
        published_at: s(&format_date_unix(rec.published_at)),
        relay_count: rec.source_relays.len() as i32,
        local: rec.local,
        on_relay: rec.relay,
    }
}

/// Populate the Keys page from local-only KP state (no relay round-trip).
/// Used at boot and after publish/rotate so the UI reflects what's on disk
/// immediately, while a relay refresh runs in the background.
/// Read the local key packages (on-disk JSON) + relay list on the backend
/// runtime, then push the rows on the UI thread.
pub(crate) fn refresh_kp_local_async(ui: &DarkMatterLinux, backend: &Arc<Backend>) {
    let weak = ui.as_weak();
    let b = backend.clone();
    backend.tokio_handle().spawn(async move {
        let local = b.key_packages_local();
        let relays = b.key_package_relays();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            // "Published" means actually out on the network — a KP with a
            // published event id, or one we've observed on a relay. A purely
            // local KP (which always exists once the account boots) does NOT
            // count.
            let published = local
                .iter()
                .any(|kp| !kp.key_package_event_id.is_empty() || kp.relay);
            ui.set_kp_published(published);
            let rows: Vec<KeyPackageInfo> = local.iter().map(kp_to_ui).collect();
            ui.set_key_packages(ModelRc::new(VecModel::from(rows)));
            let relays: Vec<SharedString> = relays.into_iter().map(SharedString::from).collect();
            ui.set_kp_relays(ModelRc::new(VecModel::from(relays)));
        });
    });
}

/// Push the on-disk relay list into the UI model. Used after add/remove.
pub(crate) fn push_network_relays(ui: &DarkMatterLinux, list: &[String]) {
    let rows: Vec<SharedString> = list.iter().cloned().map(SharedString::from).collect();
    ui.set_network_relays(ModelRc::new(VecModel::from(rows)));
    // Keep the one-click suggestions in sync: only offer ones not already added.
    push_suggested_relays(ui, list);
}

/// Well-known public relays offered as one-click adds on the get-started screen.
/// DEV POLICY: whitenoise official relays only while in development — these are
/// where the mobile apps publish, so dev peers are always mutually discoverable.
/// Before release, broaden again (e.g. relay.primal.net, relay.ditto.pub).
pub(crate) const SUGGESTED_RELAYS: &[&str] = &[
    "wss://relay.eu.whitenoise.chat",
    "wss://relay.us.whitenoise.chat",
];

/// Publish the suggested-relay chips = `SUGGESTED_RELAYS` minus whatever the user
/// already has, so a suggestion vanishes once it's added.
pub(crate) fn push_suggested_relays(ui: &DarkMatterLinux, current: &[String]) {
    let suggestions: Vec<SharedString> = SUGGESTED_RELAYS
        .iter()
        .filter(|s| !current.iter().any(|u| u.eq_ignore_ascii_case(s)))
        .map(|s| SharedString::from(*s))
        .collect();
    ui.set_suggested_relays(ModelRc::new(VecModel::from(suggestions)));
}

/// Collect a `[string]` Slint model into an owned `Vec<String>`.
pub(crate) fn vec_string_from_model(model: &ModelRc<SharedString>) -> Vec<String> {
    model.iter().map(|s| s.to_string()).collect()
}

/// Validate a user-entered relay URL. Trim is the caller's job.
pub(crate) fn validate_relay_url(url: &str) -> Result<(), String> {
    if url.is_empty() {
        return Err("Enter a relay URL.".to_string());
    }
    if !(url.starts_with("wss://") || url.starts_with("ws://")) {
        return Err("Must start with wss:// or ws://".to_string());
    }
    if url.len() < 8 {
        return Err("Relay URL looks too short.".to_string());
    }
    Ok(())
}

/// Push the booted-relays list + current health into the UI. Called after
/// the backend finishes booting.
pub(crate) fn refresh_network_post_boot(backend: &Arc<Backend>, ui: &DarkMatterLinux) {
    let booted: Vec<SharedString> = backend
        .booted_relays()
        .iter()
        .cloned()
        .map(SharedString::from)
        .collect();
    ui.set_network_booted_relays(ModelRc::new(VecModel::from(booted)));
    // `relay_health` does a `block_on` into the relay plane — poll it from a
    // worker so this post-boot UI pass never stalls the event loop.
    let weak = ui.as_weak();
    let backend = backend.clone();
    std::thread::spawn(move || {
        let (connected, total) = backend.relay_health();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_network_connected(connected as i32);
            ui.set_network_total(total as i32);
            // Mark the first sync so the chat-list footer leaves "SYNCING…"
            // and the 1s timer starts counting up from a real baseline.
            ui.set_sync_secs(0);
        });
    });
}
