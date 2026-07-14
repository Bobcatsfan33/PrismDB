#!/usr/bin/env bash
# Create a NEW golden corpus version (charter amendment C-2).
#
# It will refuse to touch an existing one. That is the entire point.
#
#   ./scripts/new-golden-corpus.sh v2 --reason "..." [--kind zipf] [--rows 2000] [--seed 1234]
#
# Golden corpora are frozen, immutable, versioned artifacts. A corpus change is a new
# version with a reviewed diff and a retained predecessor — never an edit. Every
# historical receipt (an nprobe sweep, a block-size derivation, a recall number in a
# release note) names the corpus version it was measured against, and a receipt that
# points at a corpus which no longer exists is not a receipt.
#
# Why this script exists at all: in S2, adding fields to the corpus generator shifted the
# PRNG stream and silently changed the corpus. `make-fixtures.sh` then regenerated the
# corpus AND its expected answers, so the drift check went on passing *by construction*
# while testing nothing. See D-023. Regeneration is now impossible by design: nothing but
# this script writes into testing/golden/, and this script cannot overwrite.
set -euo pipefail

cd "$(dirname "$0")/.."
PRISM="./target/release/prism"

VERSION="${1:-}"
shift || true
REASON=""
KIND="zipf"
ROWS=2000
SEED=1234
K=10

while [[ $# -gt 0 ]]; do
  case "$1" in
    --reason) REASON="$2"; shift 2 ;;
    --kind)   KIND="$2";   shift 2 ;;
    --rows)   ROWS="$2";   shift 2 ;;
    --seed)   SEED="$2";   shift 2 ;;
    --k)      K="$2";      shift 2 ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done

if [[ -z "$VERSION" || -z "$REASON" ]]; then
  echo "usage: $0 <version> --reason \"why this corpus needs to exist\" [--kind K] [--rows N] [--seed S]" >&2
  echo >&2
  echo "A new corpus version needs a reason a reviewer can evaluate. \"Refresh\" is not one." >&2
  exit 2
fi

DEST="testing/golden/$VERSION"
if [[ -e "$DEST" ]]; then
  echo "REFUSING: $DEST already exists." >&2
  echo >&2
  echo "Golden corpora are immutable (charter C-2). If this corpus needs to change, that" >&2
  echo "is a NEW version — the old one stays, because receipts point at it." >&2
  exit 1
fi

cargo build --release -q
mkdir -p "$DEST"

TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT

echo "==> generating $VERSION ($KIND, $ROWS rows, seed $SEED)"
"$PRISM" gen-corpus --kind "$KIND" --rows "$ROWS" --seed "$SEED" --out "$DEST/corpus.tsv" >/dev/null
"$PRISM" init --path "$TMP/g" --dim 64 --nlist 32 --pq-m 8 --seed "$SEED" >/dev/null
"$PRISM" ingest --path "$TMP/g" --file "$DEST/corpus.tsv" >/dev/null
"$PRISM" golden build --path "$TMP/g" --out "$DEST/expected.json" --k "$K" --kind "$KIND" >/dev/null

# --- the reviewed diff ---
PREV=$(python3 -c "import json;m=json.load(open('testing/golden/MANIFEST.json'));print(m['current'])")
echo
echo "==> diff against the current corpus ($PREV) — REVIEW THIS"
python3 - "$PREV" "$VERSION" <<'PY'
import sys
prev, new = sys.argv[1], sys.argv[2]
a = open(f"testing/golden/{prev}/corpus.tsv").read().splitlines()
b = open(f"testing/golden/{new}/corpus.tsv").read().splitlines()
same = sum(1 for x, y in zip(a, b) if x == y)
print(f"  rows: {len(a)} -> {len(b)}")
print(f"  identical rows: {same}/{min(len(a), len(b))}")
if same == min(len(a), len(b)) and len(a) == len(b):
    print()
    print("  The corpora are IDENTICAL. You do not need a new version — and if you thought")
    print("  you did, something in the generator moved that you did not intend.")
else:
    changed = min(len(a), len(b)) - same
    print(f"  CHANGED rows: {changed}")
    print()
    print("  Every receipt measured against the old corpus still names the old corpus, and")
    print("  the old corpus is still here. Nothing that was true has become untrue; the")
    print("  numbers simply describe a different corpus now. Re-derive the receipts you")
    print("  want to move forward, and say in the manifest why.")
PY

python3 - "$VERSION" "$REASON" "$KIND" "$ROWS" "$SEED" "$K" <<'PY'
import hashlib, json, os, sys
version, reason, kind, rows, seed, k = sys.argv[1:7]
def sha(p): return hashlib.sha256(open(p, "rb").read()).hexdigest()
m = json.load(open("testing/golden/MANIFEST.json"))
assert version not in m["versions"], "refusing to overwrite a frozen corpus version"
d = f"testing/golden/{version}"
m["versions"][version] = {
    "created_in_sprint": os.environ.get("PRISM_SPRINT", "unknown"),
    "generator": {"kind": kind, "rows": int(rows), "seed": int(seed)},
    "reference_config": {"dim": 64, "nlist": 32, "pq_m": 8, "seed": int(seed)},
    "k": int(k),
    "why": reason,
    "files": {
        f: {"sha256": sha(f"{d}/{f}"), "bytes": os.path.getsize(f"{d}/{f}")}
        for f in ("corpus.tsv", "expected.json")
    },
}
json.dump(m, open("testing/golden/MANIFEST.json", "w"), indent=2)
print()
print(f"==> {version} written and checksummed. The manifest still says current = {m['current']}.")
print("    Flipping `current` is a SEPARATE, deliberate edit — and it invalidates every")
print("    receipt derived from the old corpus, so re-derive them in the same change.")
PY
