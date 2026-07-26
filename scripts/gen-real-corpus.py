#!/usr/bin/env python3
"""Generate the frozen REAL-EMBEDDING golden corpus (S13 directive 1).

A small open 768d model (all-mpnet-base-v2) embeds each distinct body **once**, here, offline; the
embeddings are COMMITTED as data. Nothing at test time runs a model or the network — the Rust engine
reads `embeddings.f32` and replays them (`ReplayModelPlane`). This is an independent artifact-generation
script (like `scripts/partfmt.py`), NOT part of the shipped engine.

Air-gap (S13 dir 5): the model is downloaded once to generate; the committed artifact carries the
vectors, never a model reference the runtime resolves.

Determinism: text is drawn from a single seeded RNG (its own stream), and distinct bodies are embedded
in **sorted** order, so the artifact is reproducible given the same model. C-2: the output is frozen
under `testing/corpus/real-v1/` with a MANIFEST of SHA-256s; a new corpus is a new version directory.

Tenant-cardinality dimension (issue #7): TENANTS tenants with a mixed size distribution (a few large, a
long tail), so multi-tenant sharding / isolation / fairness are exercised at a realistic coarseness.

Run:  /tmp/s13-embed/bin/python scripts/gen-real-corpus.py
"""
import hashlib
import json
import os
import random
import sys

MODEL = "sentence-transformers/all-mpnet-base-v2"  # 768d, cosine, open
DIM = 768
ROWS = 3000
TENANTS = 64
QUERIES_PER_TOPIC = 6
SEED = 20260725
OUT = os.path.join(os.path.dirname(__file__), "..", "testing", "corpus", "real-v1")

# --- realistic agent-telemetry motifs: topic families of slotted templates -----------------------
# The geometry is the point: within a topic, bodies are semantically close; across topics, separated.
# Slots are filled per-event from large pools, so most bodies are DISTINCT (a rich continuous geometry,
# not a handful of clusters) while staying recognisably within their topic.
TOOLS = ["search", "read_file", "write_file", "execute", "http_get", "sql_query", "vector_lookup",
         "send_email", "calendar", "browser", "python", "shell", "grep", "git", "kubectl", "docker",
         "terraform", "slack", "jira", "pagerduty", "s3_get", "bigquery", "redis", "kafka_produce"]
ERRORS = ["timed out after {n} ms", "connection reset by peer", "rate limit exceeded ({n} rpm)",
          "permission denied for resource {id}", "resource {id} not found", "invalid argument {id}",
          "out of memory allocating {n} bytes", "context length exceeded by {n} tokens",
          "authentication failed for principal {id}", "quota exhausted ({n} of {n2})",
          "deadline exceeded at step {n}", "write conflict on key {id}", "TLS handshake failed",
          "DNS resolution failed for host {id}", "circuit breaker open after {n} failures"]
LANGS = ["Python", "Rust", "TypeScript", "Go", "SQL", "Bash", "Java", "Kotlin", "Ruby", "C++"]
DOCS = ["quarterly earnings report", "incident postmortem for {id}", "customer support thread {id}",
        "research paper on retrieval augmented generation", "API changelog for release {id}",
        "master services agreement", "engineering standup transcript", "product requirements document",
        "security advisory {id}", "onboarding runbook", "database migration plan {id}",
        "architecture decision record {id}"]
SUBJECTS = ["the input payload", "the user's request", "the retrieved context", "the tool result",
            "the streaming response", "the batch of records", "the retry queue", "the audit log",
            "the embedding request", "the plan graph", "the candidate passages", "the session state"]

def T(rng):  return rng.choice(TOOLS)
def L(rng):  return rng.choice(LANGS)
def S(rng):  return rng.choice(SUBJECTS)
def NID(rng): return f"{rng.randrange(1000, 99999)}"
def N(rng):  return rng.choice([8, 12, 24, 47, 128, 240, 512, 1024, 2048, 4096, 8192, 15000, 60000])

def fill(s, rng):
    return s.replace("{n2}", str(rng.choice([100, 1000, 5000]))).replace("{n}", str(N(rng))).replace("{id}", NID(rng))

# templates[topic] = list of (template, needs_rng) — each filled per event.
def render(topic, rng):
    if topic == "tool_call":
        return fill(rng.choice([
            f"calling tool {T(rng)} to act on {S(rng)}",
            f"the {T(rng)} tool returned successfully after {{n}} ms for {S(rng)}",
            f"dispatching {T(rng)} with {{n}} arguments to complete the request",
            f"tool {T(rng)} completed, writing {{n}} bytes back to {S(rng)}",
        ]), rng)
    if topic == "tool_error":
        return fill(f"the {T(rng)} tool call {rng.choice(ERRORS)} while processing {S(rng)}, retrying with backoff", rng)
    if topic == "codegen":
        return fill(rng.choice([
            f"write a {L(rng)} function that parses {S(rng)} and returns a validated struct",
            f"refactor the {L(rng)} module to remove duplicated error handling in {S(rng)}",
            f"the {L(rng)} build failed with a type error on {S(rng)} at line {{n}}",
            f"add {L(rng)} unit tests covering the edge cases in {S(rng)}",
        ]), rng)
    if topic == "retrieval":
        return fill(rng.choice([
            f"retrieved {{n}} documents from the vector index for {S(rng)}",
            f"the semantic search over {S(rng)} returned {{n}} results above the similarity threshold",
            f"reranking {{n}} candidate passages by cross-encoder score for {S(rng)}",
            f"{S(rng)} exceeded the model context window by {{n}} tokens and was truncated",
        ]), rng)
    if topic == "planning":
        return fill(rng.choice([
            f"decompose the task into {{n}} subgoals and execute them in order",
            f"the agent revised its plan for {S(rng)} after the {T(rng)} call failed",
            f"waiting on step {{n}} to complete before proceeding with {S(rng)}",
            f"the planner selected {T(rng)} to gather more information about {S(rng)}",
        ]), rng)
    if topic == "summarize":
        return fill(rng.choice([
            f"summarize the {rng.choice(DOCS)} into {{n}} bullet points",
            f"extract the action items from the {rng.choice(DOCS)}",
            f"draft a reply to the {rng.choice(DOCS)} referencing {S(rng)}",
        ]), rng)
    if topic == "auth":
        return fill(rng.choice([
            f"the request for {S(rng)} was rejected because the bearer token for principal {{id}} had expired",
            f"rotating the service credential {{id}} after a suspected leak",
            f"tenant {{id}} is not authorized to access {S(rng)}",
            f"multi-factor challenge issued to principal {{id}} for {S(rng)}",
        ]), rng)
    if topic == "llm_span":
        return fill(rng.choice([
            f"llm completion span: {{n}} prompt tokens, {{n}} completion tokens on {S(rng)}",
            f"model latency {{n}} ms at temperature 0.7 for {S(rng)}",
            f"streaming {{n}} tokens for {S(rng)}, first token at {{n2}} ms",
        ]), rng)
    if topic == "sql":
        return fill(rng.choice([
            f"SELECT count(*) FROM events WHERE tenant_id = 't{{id}}' AND event_time > {{n}}",
            f"the query plan for {S(rng)} chose a full table scan over the index",
            f"a slow query on {S(rng)} held a lock for {{n}} ms, past the timeout",
            f"aggregating {S(rng)} by tenant and by hour over {{n}} rows",
        ]), rng)
    if topic == "safety":
        return fill(rng.choice([
            f"{S(rng)} was flagged by the content filter and blocked",
            f"redacting personally identifiable information from {S(rng)} before embedding",
            f"the output for {S(rng)} was regenerated after a policy violation",
            f"an injection attempt was detected in {S(rng)} at offset {{n}}",
        ]), rng)
    raise ValueError(topic)

TOPICS = ["tool_call", "tool_error", "codegen", "retrieval", "planning",
          "summarize", "auth", "llm_span", "sql", "safety"]

def main():
    rng = random.Random(SEED)
    topics = TOPICS
    # Topic weights: a Zipf-ish skew so a few topics dominate (realistic).
    weights = [1.0 / (i + 1) for i in range(len(topics))]

    # Tenant sizes: mixed distribution — a few large tenants, a long tail (issue #7).
    tenant_weights = [1.0 / (i + 1) ** 0.8 for i in range(TENANTS)]

    events = []
    base_time = 1_760_000_000_000
    for i in range(ROWS):
        topic = rng.choices(topics, weights=weights, k=1)[0]
        body = render(topic, rng)
        t = rng.choices(range(TENANTS), weights=tenant_weights, k=1)[0]
        events.append({
            "event_id": f"e{i:06d}",
            "tenant_id": f"t{t:03d}",
            "event_time": base_time + i * 1000,
            "observed_time": base_time + i * 1000,
            "event_name": topic,
            "cost": round(rng.uniform(0.0, 0.05), 6),
            "error": topic in ("tool_error", "auth", "safety") and rng.random() < 0.5,
            "body": body,
        })

    # Queries: fresh renders per topic (their own texts, embedded like bodies) — realistic queries a
    # user would issue, distinct from but semantically near their topic's rows.
    queries = []
    for topic in topics:
        seen = set()
        while len(seen) < QUERIES_PER_TOPIC:
            q = render(topic, rng)
            if q not in seen:
                seen.add(q)
                queries.append({"topic": topic, "text": q})

    # Distinct texts to embed = all bodies used + all query texts, sorted for determinism.
    distinct = sorted(set(e["body"] for e in events) | set(q["text"] for q in queries))
    print(f"rows={len(events)} tenants={TENANTS} topics={len(topics)} distinct_bodies={len(distinct)} queries={len(queries)}", file=sys.stderr)

    # --- embed once, offline ---
    from sentence_transformers import SentenceTransformer
    model = SentenceTransformer(MODEL, device="cpu")
    import numpy as np
    embs = model.encode(distinct, batch_size=64, normalize_embeddings=True, show_progress_bar=True)
    embs = np.asarray(embs, dtype=np.float32)
    assert embs.shape == (len(distinct), DIM), embs.shape

    os.makedirs(OUT, exist_ok=True)
    # bodies.txt — one distinct text per line (newlines in bodies are disallowed by construction).
    with open(os.path.join(OUT, "bodies.txt"), "w") as f:
        for b in distinct:
            assert "\n" not in b
            f.write(b + "\n")
    # embeddings.f32 — raw little-endian f32, row-major, row i <-> bodies.txt line i.
    with open(os.path.join(OUT, "embeddings.f32"), "wb") as f:
        f.write(embs.tobytes())
    # events.jsonl
    with open(os.path.join(OUT, "events.jsonl"), "w") as f:
        for e in events:
            f.write(json.dumps(e) + "\n")
    # queries.jsonl
    with open(os.path.join(OUT, "queries.jsonl"), "w") as f:
        for q in queries:
            f.write(json.dumps(q) + "\n")

    def sha(p):
        h = hashlib.sha256()
        with open(os.path.join(OUT, p), "rb") as fh:
            h.update(fh.read())
        return h.hexdigest()

    manifest = {
        "corpus_version": "real-v1",
        "model": MODEL,
        "dim": DIM,
        "normalized": True,
        "rows": len(events),
        "tenants": TENANTS,
        "topics": topics,
        "distinct_bodies": len(distinct),
        "queries": len(queries),
        "seed": SEED,
        "generated_by": "scripts/gen-real-corpus.py",
        "note": "Embeddings generated ONCE and committed as data; nothing at test time runs a model or the network (S13 dir 1/5). C-2 frozen; a new corpus is a new version directory.",
        "sha256": {p: sha(p) for p in ("bodies.txt", "embeddings.f32", "events.jsonl", "queries.jsonl")},
    }
    with open(os.path.join(OUT, "MANIFEST.json"), "w") as f:
        json.dump(manifest, f, indent=2)
    print("wrote", OUT, file=sys.stderr)

if __name__ == "__main__":
    main()
