# CI forensics — getting evidence out of a run

Notes for reading CI when the obvious command returns nothing. Each entry exists because it cost
someone real time once.

---

## `gh run view --log` returns zero bytes

**Symptom.** A job is green (or red) and you want the numbers it printed. `gh run view <id> --log`
succeeds and emits **nothing** — zero bytes, no error. Piping to `grep` finds nothing, so it looks
like the job printed nothing at all.

**Cause.** The log contains terminal escape sequences (colour codes from `cargo`, and the runner's
own group markers). `gh` refuses to write them to a non-TTY by default and, rather than failing
loudly, produces an empty stream. The refusal message only appears on stderr for the API route:

```
the response contains terminal escape sequences; pass --allow-escape-sequences to output it anyway
```

**Fix.** Go through the API for the specific job, allow escape sequences, and strip them:

```bash
# the job id, not the run id
JID=$(gh run view <run-id> --json jobs -q '.jobs[] | select(.name|test("<name fragment>")) | .databaseId')

gh api "repos/<owner>/<repo>/actions/jobs/$JID/logs" --allow-escape-sequences > /tmp/job.log
grep -a "<marker>" /tmp/job.log | sed 's/\x1b\[[0-9;]*m//g'
```

`grep -a` matters: the log is classified as binary once escape sequences are in it, and plain `grep`
silently reports "binary file matches" instead of the line.

**Why this is written down.** An S14 report was filed with a disclaimer that the CI-side figures
could not be retrieved, when the numbers were there the whole time behind one flag. A missing number
is worth a disclaimer; a *retrievable* number behind an undiscovered flag is worth a note.

**Do not** substitute a local run for a CI number without labelling it as such. A local figure and a
CI figure are different measurements on different hardware — the S12 commit-RTT gate reads
p50 ≈ 0.6 ms locally and ≈ 1.6 ms on the runner, and the receipt names which one produced it.

---

## A job you expect to exist has no runs at all

**Symptom.** `gh run list --commit <sha>` returns nothing for a branch that has commits.

**Cause, usually.** The workflow's triggers do not include pushes to that branch. `ci.yml` fires on
`push: branches: [main]`, `pull_request`, and `workflow_dispatch` — so a branch pushed **without a
PR** has never been tested, however many commits it carries.

**Fix.** Dispatch it explicitly against the branch ref before relying on it:

```bash
gh workflow run ci.yml --ref <branch>
gh run list --branch <branch> --limit 5
```

Confirm the run's `headSha` is the commit you meant:

```bash
gh run view <id> --json name,headSha,status,conclusion
```

**Why this is written down.** The whole S14 encryption sprint — 15 commits — reached its merge
review having never once been through CI, because it lived on a branch with no PR. The absence of a
run reads exactly like a passing run if nobody looks.

---

## Step-level conclusions when per-test counts are unavailable

`gh run view <id> --json jobs -q '.jobs[] | select(...) | .steps[] | "\(.conclusion)\t\(.name)"'`
gives a per-step verdict without touching logs. It proves a step **ran and passed**; it does not
prove how many tests ran inside it. If the distinction matters — and for a gate with a name filter
it always does, since a filter matching nothing exits zero — read the log for
`running N tests` with N > 0, or assert the count inside the test itself.
