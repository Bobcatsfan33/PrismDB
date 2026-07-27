#!/usr/bin/env python3
"""Append **cluster-boundary queries** to the frozen real-v1 corpus (S13 directive 2).

The nprobe default is derived on the TAIL (p1/p5), because a mean hides the query class that fails
completely: cluster-boundary queries, whose true neighbours are split across two centroids so a small
nprobe reaches one and misses the other (the S0/S1 `min recall = 0.000` lesson). real-v1 shipped with
topic queries only; this adds boundary queries so the nprobe re-derivation has real boundaries to stand
on — the boundary-query lesson from S1, now on continuous 768d geometry instead of the hash motifs.

**Append-only, by construction.** It reads the existing `bodies.txt`/`embeddings.f32` and does NOT
touch a single existing row: it embeds ONLY the new boundary sentences and appends them (new lines in
`bodies.txt`, new rows in `embeddings.f32`). Every prior receipt (ε, drift, generation) stays
byte-valid because the existing vectors are unchanged. A separate `boundary_queries.jsonl` keeps the
boundary set distinct from the topic queries, so `queries.jsonl`-driven receipts (ε) are untouched.

A boundary sentence = first half of a fresh render of topic A + second half of a fresh render of topic
B (the continuous analogue of the hash oracle's half-A/half-B construction, `oracle.rs::standard_queries`).

Run:  HF_HUB_OFFLINE=1 /tmp/s13-embed/bin/python scripts/gen-real-corpus-boundary.py
"""
import hashlib
import itertools
import json
import os
import random
import sys

# Reuse the exact render()/TOPICS/fill() machinery so the boundary halves are the same telemetry text.
sys.path.insert(0, os.path.dirname(__file__))
import importlib.util

_spec = importlib.util.spec_from_file_location(
    "gen_real_corpus", os.path.join(os.path.dirname(__file__), "gen-real-corpus.py")
)
_g = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_g)

DIM = _g.DIM
MODEL = _g.MODEL
OUT = _g.OUT
BOUNDARY_SEED = 20260727  # its own RNG stream (C-2), distinct from the corpus seed


def boundary_text(a, b, rng):
    """First half of a render of topic `a` + second half of a render of topic `b`."""
    pa = _g.render(a, rng).split()
    pb = _g.render(b, rng).split()
    half_a = pa[: (len(pa) + 1) // 2]
    half_b = pb[len(pb) // 2 :]
    return " ".join(half_a + half_b)


def main():
    rng = random.Random(BOUNDARY_SEED)
    topics = _g.TOPICS

    # One boundary query per unordered topic pair, plus its mirror (so neither topic is systematically
    # the leading one). C(10,2)=45 pairs -> 90 boundary queries.
    boundary = []
    for a, b in itertools.combinations(topics, 2):
        boundary.append({"topic_a": a, "topic_b": b, "text": boundary_text(a, b, rng)})
        boundary.append({"topic_a": b, "topic_b": a, "text": boundary_text(b, a, rng)})

    # De-dup boundary texts (a rare collision would break the query->text mapping).
    seen, uniq = set(), []
    for q in boundary:
        if q["text"] not in seen:
            seen.add(q["text"])
            uniq.append(q)
    boundary = uniq
    print(f"boundary queries: {len(boundary)}", file=sys.stderr)

    # --- read existing bodies (NOT modified) ---
    with open(os.path.join(OUT, "bodies.txt")) as f:
        existing = [ln.rstrip("\n") for ln in f]
    existing_set = set(existing)

    # New texts to embed = boundary texts not already present.
    new_texts = sorted({q["text"] for q in boundary} - existing_set)
    print(f"new texts to embed+append: {len(new_texts)} (of {len(boundary)} boundary queries)", file=sys.stderr)
    for q in boundary:
        assert "\n" not in q["text"]

    if new_texts:
        from sentence_transformers import SentenceTransformer
        import numpy as np

        model = SentenceTransformer(MODEL, device="cpu")
        embs = model.encode(new_texts, batch_size=64, normalize_embeddings=True, show_progress_bar=True)
        embs = np.asarray(embs, dtype=np.float32)
        assert embs.shape == (len(new_texts), DIM), embs.shape

        # APPEND — existing rows are byte-identical; only new lines/rows are added.
        with open(os.path.join(OUT, "bodies.txt"), "a") as f:
            for t in new_texts:
                f.write(t + "\n")
        with open(os.path.join(OUT, "embeddings.f32"), "ab") as f:
            f.write(embs.tobytes())

    with open(os.path.join(OUT, "boundary_queries.jsonl"), "w") as f:
        for q in boundary:
            f.write(json.dumps(q) + "\n")

    def sha(p):
        h = hashlib.sha256()
        with open(os.path.join(OUT, p), "rb") as fh:
            h.update(fh.read())
        return h.hexdigest()

    # Update the manifest: refresh SHAs (bodies/embeddings grew by append; boundary_queries is new).
    with open(os.path.join(OUT, "MANIFEST.json")) as f:
        manifest = json.load(f)
    with open(os.path.join(OUT, "bodies.txt")) as f:
        total_bodies = sum(1 for _ in f)
    manifest["distinct_bodies"] = total_bodies
    manifest["boundary_queries"] = len(boundary)
    manifest["boundary_seed"] = BOUNDARY_SEED
    manifest["boundary_note"] = (
        "Cluster-boundary queries (S13 dir 2) appended by scripts/gen-real-corpus-boundary.py: "
        "half a render of topic A + half a render of topic B, embedded by the same model and APPENDED "
        "so every existing row (and thus every prior receipt) is byte-identical. Drives the nprobe/"
        "adaptive tail (p1/p5) re-derivation; kept separate from queries.jsonl so ε is untouched."
    )
    manifest["sha256"] = {
        p: sha(p)
        for p in ("bodies.txt", "embeddings.f32", "events.jsonl", "queries.jsonl", "boundary_queries.jsonl")
    }
    with open(os.path.join(OUT, "MANIFEST.json"), "w") as f:
        json.dump(manifest, f, indent=2)
    print("updated", OUT, "total_bodies=", total_bodies, file=sys.stderr)


if __name__ == "__main__":
    main()
