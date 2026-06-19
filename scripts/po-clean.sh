#!/usr/bin/env bash
# Normalize a gettext PO/POT stream read on stdin, write the result to stdout.
#
# Used in two places, so the rules live here once:
#   * the git `po-clean` clean filter (.gitattributes -> registered by
#     scripts/install-hooks.sh), which runs on every `git add`;
#   * the .githooks/pre-commit hook, which also rewrites the working tree.
#
# It MUST be idempotent — running it twice must yield byte-identical output —
# or it would itself create the phantom diffs it exists to prevent.
#
#   --no-location   drop "#: file:line" refs so UI source-line shifts never
#                   surface as catalog diffs
#   --sort-output   stable order keyed on msgid. NOT --sort-by-file: once
#                   --no-location strips the file refs, sort-by-file has no key
#                   left and the order flips on the next pass (not idempotent).
#   --no-wrap       one string per line; no width-dependent rewrapping
#   sed             drop the volatile POT-Creation-Date header line
set -euo pipefail

if command -v msgcat >/dev/null 2>&1; then
    msgcat --no-location --no-wrap --sort-output -o - - | sed '/^"POT-Creation-Date:/d'
else
    # gettext not installed: still strip the one always-changing header line so
    # the most common phantom-diff case is handled without a hard dependency.
    sed '/^"POT-Creation-Date:/d'
fi
