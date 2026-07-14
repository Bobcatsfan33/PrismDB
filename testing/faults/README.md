# Fault injection — permanent artifact #3

> *"A fault-injection matrix that kills writers at every durability boundary."*
> — PRISM.md, Part II §7.4

Every durability boundary in the write path is a **named kill point**. Ask the
binary what they are:

```bash
prism kill-points
```

Setting `PRISM_FAULT=<point>` makes the process `abort()` when it reaches that
boundary — no destructors, no flushes, no tidying up. That is what a crash is.

## The two harnesses

**The matrix** — `crates/prism-cli/tests/faults.rs`. Walks every declared kill
point, a few times each, and asserts the four things that must be true after any
crash: the store still opens; the live snapshot is the old one or the new one and
never a hybrid; every part the snapshot names is still checksum- *and*
structure-valid; and the store still accepts writes and answers queries. Runs on
every commit.

```bash
cargo test --release -p prism-cli --test faults
```

**The campaign** — `crates/prism-cli/tests/campaign.rs`. The S1 acceptance gate.
It does not know where the bug is: it kills the writer at a *randomly chosen*
boundary, over and over, on a store that keeps accumulating real history, and
insists that after every single death the store is one of exactly two things.

```bash
# the gate: 10,000 runs, ~11 minutes
PRISM_CAMPAIGN_RUNS=10000 cargo test --release -p prism-cli --test campaign -- --ignored --nocapture
```

CI runs 400 on every commit and the full 10,000 nightly.

## What the campaign proves, beyond "it didn't crash"

The commit counts are the interesting output. Of 300 crashes in a sample run,
**52 committed — and exactly 52 were kills at `current.after_rename`.** Every kill
at any *earlier* boundary rolled back, without exception.

That is the atomicity claim, measured rather than asserted: publication happens at
one instant, the rename of `CURRENT`, and a crash on either side of it leaves a
complete world. Everything written before that instant and abandoned is an
**orphan** — real bytes, checksum-valid, named by no snapshot, visible to no
reader, and reclaimable only by GC.
