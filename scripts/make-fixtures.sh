#!/usr/bin/env bash
# Regenerate the permanent test artifacts (docs/PRISM.md, Part II §7.4).
#
#   testing/compat/  — parts written by every released format version. These are
#                      committed *bytes*, not a generator: the whole point is to
#                      prove that today's build still opens yesterday's data.
#   testing/golden/  — the exact-search corpus and its brute-force ground truth.
#   testing/faults/  — driven by the fault harness; nothing to generate here.
#
# Run this only when the format version is bumped or the corpus deliberately
# changes. If it produces a diff on an unchanged format, that is a bug, not a
# refresh: something non-deterministic got into the write path.
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT=$(pwd)
PRISM="$ROOT/target/release/prism"

cargo build --release -q

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# ---------------------------------------------------------------- compat, v1
# Small on purpose: a fixture is read by every future version forever, so it
# should be cheap to carry. 200 rows, dim 32, pq_m 4.
echo "==> testing/compat/v1"
rm -rf testing/compat/v1
mkdir -p testing/compat
"$PRISM" gen-corpus --kind uniform --rows 200 --seed 7 --out "$TMP/compat.tsv" >/dev/null
"$PRISM" init --path testing/compat/v1 --dim 32 --nlist 8 --pq-m 4 --seed 7 >/dev/null
"$PRISM" ingest --path testing/compat/v1 --file "$TMP/compat.tsv" >/dev/null
cp "$TMP/compat.tsv" testing/compat/v1-source.tsv
"$PRISM" verify --path testing/compat/v1

# ------------------------------------------------------------ compat, corrupt
# Each of these must be rejected with a *specific* error, not a generic read
# failure. A database that cannot tell you which byte is wrong will be blamed
# for corruption it did not cause.
echo "==> testing/compat/corrupt"
rm -rf testing/compat/corrupt
mkdir -p testing/compat/corrupt

PART=$(ls testing/compat/v1/parts | head -1)

# 1. A single flipped byte inside the PQ codes: checksum must catch it.
cp -R testing/compat/v1 testing/compat/corrupt/flipped-byte
python3 - "$PWD/testing/compat/corrupt/flipped-byte/parts/$PART/pq.codes" <<'PY'
import sys
p = sys.argv[1]
b = bytearray(open(p, 'rb').read())
b[len(b)//2] ^= 0x01
open(p, 'wb').write(b)
PY

# 2. A truncated column: the declared length no longer matches the bytes.
cp -R testing/compat/v1 testing/compat/corrupt/truncated-column
python3 - "$PWD/testing/compat/corrupt/truncated-column/parts/$PART/vectors.f32" <<'PY'
import sys
p = sys.argv[1]
b = open(p, 'rb').read()
open(p, 'wb').write(b[: len(b) // 2])
PY

# 3. A format version from the future: refuse rather than guess.
cp -R testing/compat/v1 testing/compat/corrupt/future-format
python3 - "$PWD/testing/compat/corrupt/future-format/parts/$PART/manifest.json" <<'PY'
import json, sys
p = sys.argv[1]
m = json.load(open(p))
m["format_version"] = 999
json.dump(m, open(p, "w"), indent=2)
PY

# 4. A codebook edited in place: the generation no longer hashes to its own id,
#    which means every code byte in every part just changed meaning.
cp -R testing/compat/v1 testing/compat/corrupt/mutated-codebook
python3 - "$PWD/testing/compat/corrupt/mutated-codebook" <<'PY'
import json, os, sys
root = sys.argv[1]
gens = os.path.join(root, "generations")
f = os.path.join(gens, os.listdir(gens)[0])
g = json.load(open(f))
g["coarse"]["centroids"][0] += 0.5
json.dump(g, open(f, "w"), indent=2)
PY

# 5. A string offset that points past the end of its blob: must not index out of
#    bounds and must not allocate on trust.
cp -R testing/compat/v1 testing/compat/corrupt/bad-offsets
python3 - "$PWD/testing/compat/corrupt/bad-offsets/parts/$PART" <<'PY'
import json, os, struct, sys, zlib
part = sys.argv[1]
off = os.path.join(part, "body.off")
b = bytearray(open(off, 'rb').read())
struct.pack_into("<q", b, 8, 1 << 40)   # second offset -> 1 TiB into a small blob
open(off, 'wb').write(b)
# Fix the checksum so the *offset* validation is what rejects this, not the CRC.
m = json.load(open(os.path.join(part, "manifest.json")))
for c in m["columns"]:
    if c["name"] == "body.offsets":
        c["crc32"] = zlib.crc32(bytes(b)) & 0xFFFFFFFF
json.dump(m, open(os.path.join(part, "manifest.json"), "w"), indent=2)
PY

# ---------------------------------------------------------------- golden corpus
# The corpus and the exact top-k for every query, computed by brute force over
# every stored vector. Committed as a TSV plus expectations; the store itself is
# rebuilt deterministically at test time, so we carry text, not megabytes of
# float32.
echo "==> testing/golden"
mkdir -p testing/golden
"$PRISM" gen-corpus --kind zipf --rows 2000 --seed 1234 --out testing/golden/corpus.tsv >/dev/null
"$PRISM" init --path "$TMP/golden" --dim 64 --nlist 32 --pq-m 8 --seed 1234 >/dev/null
"$PRISM" ingest --path "$TMP/golden" --file testing/golden/corpus.tsv >/dev/null
"$PRISM" golden build --path "$TMP/golden" --out testing/golden/expected.json --k 10 --kind zipf

echo
echo "Fixtures regenerated. Review the diff: on an unchanged format there should be none."
