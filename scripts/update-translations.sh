#!/usr/bin/env bash
# Extract @tr strings from Slint UI files and merge into locale catalogs.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# The gettext domain must match the crate that compiles the Slint UI
# (slint-build hardwires it to CARGO_PKG_NAME) — that is dm-ui, not the app.
POT="$ROOT/lang/dm-ui.pot"
DOMAIN="dm-ui"

if ! command -v slint-tr-extractor >/dev/null 2>&1; then
    echo "slint-tr-extractor not found — install with: cargo install slint-tr-extractor" >&2
    exit 1
fi

mkdir -p lang/{en,it,de,ja}/LC_MESSAGES

find ui -name '*.slint' -print0 | sort -z | xargs -0 slint-tr-extractor -o "$POT"

if ! command -v msgmerge >/dev/null 2>&1; then
    echo "msgmerge not found — install gettext (e.g. pacman -S gettext)" >&2
    exit 1
fi

for locale in it de ja; do
    PO="lang/$locale/LC_MESSAGES/$DOMAIN.po"
    if [[ ! -f "$PO" ]]; then
        msginit --no-translator --locale="$locale" --input="$POT" --output-file="$PO"
    else
        msgmerge -U "$PO" "$POT"
    fi
done

echo "Updated $POT and merged into it/de/ja catalogs."
