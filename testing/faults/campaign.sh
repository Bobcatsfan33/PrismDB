#!/usr/bin/env bash
# The randomized kill/reopen campaign.
#
# The fault *matrix* (crates/prism-cli/tests/faults.rs) walks every declared kill
# point a few times and runs in CI on every commit. This is the long version: it
# kills the writer at a randomly chosen boundary, over and over, and after each
# death asserts the store opens to the old snapshot or the new one and never to
# a hybrid of the two.
#
# The S1 acceptance gate is 10,000 runs. S0 ships the harness; S1 turns the
# number up and puts it in a nightly job.
#
#   ./testing/faults/campaign.sh 10000
set -euo pipefail

RUNS="${1:-200}"
cd "$(dirname "$0")/../.."
ROOT=$(pwd)
PRISM="$ROOT/target/release/prism"

cargo build --release -q

POINTS=($("$PRISM" kill-points | python3 -c 'import json,sys; print(" ".join(json.load(sys.stdin)))'))
echo "campaign: $RUNS runs over ${#POINTS[@]} kill points"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

"$PRISM" gen-corpus --kind zipf --rows 300 --seed 1 --out "$WORK/a.tsv" >/dev/null
"$PRISM" gen-corpus --kind zipf --rows 300 --seed 2 --out "$WORK/b.tsv" >/dev/null

hybrid=0
survived=0

for ((i = 1; i <= RUNS; i++)); do
  STORE="$WORK/s$i"
  rm -rf "$STORE"

  "$PRISM" init --path "$STORE" --dim 32 --nlist 8 --pq-m 4 >/dev/null
  "$PRISM" ingest --path "$STORE" --file "$WORK/a.tsv" >/dev/null
  BEFORE=$("$PRISM" inspect --path "$STORE" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["rows"])')

  POINT=${POINTS[$((RANDOM % ${#POINTS[@]}))]}

  # The kill is expected. A clean exit at a write boundary would itself be a bug.
  set +e
  PRISM_FAULT="$POINT" "$PRISM" ingest --path "$STORE" --file "$WORK/b.tsv" >/dev/null 2>&1
  set -e

  # The only two legal outcomes.
  if ! "$PRISM" verify --path "$STORE" >/dev/null 2>&1; then
    echo "FAIL run $i at $POINT: the store does not verify after a crash"
    hybrid=$((hybrid + 1))
    continue
  fi
  AFTER=$("$PRISM" inspect --path "$STORE" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["rows"])')

  if [[ "$AFTER" != "$BEFORE" && "$AFTER" != "$((BEFORE + 300))" ]]; then
    echo "FAIL run $i at $POINT: hybrid state — $BEFORE rows before, $AFTER after"
    hybrid=$((hybrid + 1))
    continue
  fi

  survived=$((survived + 1))
  rm -rf "$STORE"
  if ((i % 50 == 0)); then echo "  $i/$RUNS ok"; fi
done

echo
echo "runs: $RUNS   clean: $survived   hybrid/corrupt: $hybrid"
[[ $hybrid -eq 0 ]] || { echo "CAMPAIGN FAILED"; exit 1; }
echo "campaign clean: every crash left the old snapshot or the new one."
