#!/usr/bin/env bash
# Activate the project's git hooks and the po-clean catalog filter for this
# clone. Run once after cloning:
#
#     scripts/install-hooks.sh
#
# Git hooks and per-repo filters can't be committed, so each clone registers
# them locally. Both settings are scoped to this repository only.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# Use the tracked .githooks/ directory instead of .git/hooks/.
git config core.hooksPath .githooks

# Register the clean filter referenced by .gitattributes. It is clean-only
# (normalize on `git add`); smudge is a pass-through so checkouts stay
# byte-identical to what is stored.
git config filter.po-clean.clean  'scripts/po-clean.sh'
git config filter.po-clean.smudge cat

echo "✓ core.hooksPath -> .githooks"
echo "✓ filter.po-clean registered (PO/POT normalized on stage)"
echo
echo "Hooks are now active for this clone. Bypass once with: git commit --no-verify"
