#!/usr/bin/env bash
# Run the README's quickstart. Literally.
#
# Not a copy of the quickstart, not a script the quickstart is "based on": this
# extracts the shell block out of README.md and executes it. If a command in the
# README does not work, this fails, and CI is red. A quickstart that does not run
# is worse than no quickstart, because it costs the reader their first hour and
# their trust.
set -euo pipefail

cd "$(dirname "$0")/.."

BLOCK=$(mktemp)
trap 'rm -f "$BLOCK"; rm -rf ./demo ./events.tsv' EXIT

python3 - README.md "$BLOCK" <<'PY'
import re, sys

readme, out = sys.argv[1], sys.argv[2]
text = open(readme).read()

# Everything fenced as ```bash between "## Quickstart" and the next H2.
m = re.search(r"^## Quickstart\s*$(.*?)^## ", text, re.M | re.S)
if not m:
    sys.exit("README.md has no '## Quickstart' section")

blocks = re.findall(r"```bash\n(.*?)```", m.group(1), re.S)
if not blocks:
    sys.exit("the Quickstart section has no ```bash block")

with open(out, "w") as f:
    f.write("set -euxo pipefail\n")
    for b in blocks:
        f.write(b)
        f.write("\n")

print(f"extracted {len(blocks)} block(s), {sum(b.count(chr(10)) for b in blocks)} lines")
PY

rm -rf ./demo ./events.tsv
bash "$BLOCK"

echo
echo "README quickstart ran clean."
