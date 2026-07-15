#!/usr/bin/env bash
# Every `unsafe` block in the shipping crates must be documented in docs/UNSAFE-INVENTORY.md
# (S6, determinism contract §6). This is grep-level enforcement: it does not understand the safety
# arguments, it only guarantees that ADDING an `unsafe` token forces a diff to the inventory, where
# a reviewer will see the argument. Crude on purpose.
set -euo pipefail
cd "$(dirname "$0")/.."

# Count `unsafe` tokens in the shipping source (not tests, not this script, not docs).
actual=$(grep -rEo '\bunsafe\b' crates/*/src --include='*.rs' | wc -l | tr -d ' ')

# The number the inventory claims, from its "Total ... : N." line.
claimed=$(grep -oE 'Total `unsafe` tokens in `crates/\*/src`: [0-9]+' docs/UNSAFE-INVENTORY.md \
  | grep -oE '[0-9]+' | head -1)

echo "unsafe tokens in crates/*/src: $actual"
echo "documented in UNSAFE-INVENTORY.md: ${claimed:-<none>}"

if [ -z "${claimed:-}" ]; then
  echo "FAIL: docs/UNSAFE-INVENTORY.md has no 'Total ...' line to check against." >&2
  exit 1
fi

if [ "$actual" != "$claimed" ]; then
  echo >&2
  echo "FAIL: the source has $actual unsafe tokens but the inventory documents $claimed." >&2
  echo "An unsafe block without an inventory entry is a safety promise nobody wrote down." >&2
  echo "Update docs/UNSAFE-INVENTORY.md -- the table AND the total -- in this change." >&2
  echo >&2
  echo "The unsafe tokens are:" >&2
  grep -rn '\bunsafe\b' crates/*/src --include='*.rs' >&2
  exit 1
fi

echo "ok: every unsafe block is accounted for."
