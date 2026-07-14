//! Charter amendment C-1, enforced.
//!
//! > **Every tuned constant must be pinned to committed benchmark evidence, with a
//! > test asserting the constant still matches that evidence.**
//!
//! This is that test. It runs on every commit, and it checks the ledger and the code
//! against each other **in both directions** — so a constant cannot drift away from
//! its receipt, and a new constant cannot be smuggled in without one.

use prism_engine::tuning::{constants, Kind, Registry};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn registry() -> Registry {
    let path = repo_root().join("testing/evidence/registry.json");
    let bytes = std::fs::read(&path).expect("the constant ledger must exist");
    serde_json::from_slice(&bytes).expect("the constant ledger must parse")
}

#[test]
fn every_tuned_constant_matches_its_committed_evidence() {
    let reg = registry();

    for c in constants() {
        let entry = reg
            .constants
            .iter()
            .find(|e| e.name == c.name)
            .unwrap_or_else(|| {
                panic!(
                    "constant `{}` is not in testing/evidence/registry.json. Charter C-1: every \
                     constant that steers behaviour is registered, and a tuned one owes evidence. \
                     Add it to the ledger — and if it is `tuned`, derive it before you do.",
                    c.name
                )
            });

        assert_eq!(
            entry.value, c.value,
            "constant `{}` is {} in the code but {} in the ledger. One of them is stale. If the \
             ledger is right, fix the code; if the code is right, RE-DERIVE the evidence — do not \
             just edit the number.",
            c.name, c.value, entry.value
        );
        assert_eq!(
            entry.kind, c.kind,
            "constant `{}` is classified differently in the ledger",
            c.name
        );

        match c.kind {
            Kind::Tuned => {
                // A tuned constant owes evidence: a file, a key inside it, and the
                // rule by which that key was chosen.
                let ev = entry.evidence.as_ref().unwrap_or_else(|| {
                    panic!(
                        "tuned constant `{}` has no evidence file in the ledger",
                        c.name
                    )
                });
                let key = entry.evidence_key.as_ref().unwrap_or_else(|| {
                    panic!(
                        "tuned constant `{}` names no key inside its evidence",
                        c.name
                    )
                });
                assert!(
                    entry.rule.as_ref().is_some_and(|r| r.len() > 40),
                    "tuned constant `{}` has no rule explaining how the evidence chose it. \
                     Evidence without a rule is a number in a file.",
                    c.name
                );

                let path = repo_root().join(ev);
                assert!(
                    path.exists(),
                    "tuned constant `{}` points at evidence `{ev}`, which does not exist",
                    c.name
                );

                // The receipt must actually say the value.
                let doc: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                let from_evidence = doc
                    .get(key)
                    .and_then(|v| v.as_i64())
                    .unwrap_or_else(|| panic!("evidence `{ev}` has no integer key `{key}`"));

                assert_eq!(
                    from_evidence, c.value,
                    "constant `{}` is {} in the code, but its own evidence (`{ev}` -> `{key}`) \
                     chose {from_evidence}. The constant has drifted away from the measurement \
                     that justified it.",
                    c.name, c.value
                );
            }
            Kind::Policy => {
                // A policy constant owes a rationale that points at prose — and the
                // prose has to exist. This is what stops `policy` becoming the
                // laundering route for every constant somebody could not be bothered
                // to derive.
                let r = entry.rationale.as_ref().unwrap_or_else(|| {
                    panic!(
                        "policy constant `{}` has no rationale. A policy is a decision about \
                         behaviour, and a decision nobody wrote down is not a decision.",
                        c.name
                    )
                });
                assert!(
                    r.len() > 30,
                    "policy constant `{}` has a rationale too thin to review: {r}",
                    c.name
                );

                // The pointer has to resolve. "See the docs" is not a rationale if the
                // docs do not exist.
                if let Some(doc) = r.split_whitespace().next() {
                    if doc.ends_with(".md") || doc.contains(".md") {
                        let file = doc.split('#').next().unwrap();
                        assert!(
                            repo_root().join(file).exists(),
                            "policy constant `{}` points at `{file}`, which does not exist",
                            c.name
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn the_ledger_holds_no_constant_the_code_does_not() {
    // The other direction. A stale ledger entry is a constant somebody deleted and
    // forgot to un-register, and it makes the ledger a place where things go to rot.
    let reg = registry();
    let known: BTreeSet<&str> = constants().iter().map(|c| c.name).collect();

    for e in &reg.constants {
        assert!(
            known.contains(e.name.as_str()),
            "the ledger registers `{}`, which no longer exists in the code",
            e.name
        );
    }
    assert_eq!(reg.constants.len(), constants().len());
}

#[test]
fn the_block_size_evidence_actually_justifies_the_choice() {
    // Not just "the number in the file matches the number in the code" — the *rule*
    // the evidence claims to have followed must actually select that number from the
    // data it recorded. Otherwise the receipt is decorative.
    let doc: serde_json::Value = serde_json::from_slice(
        &std::fs::read(repo_root().join("testing/evidence/block-size.json")).unwrap(),
    )
    .unwrap();

    let chosen = doc["chosen_block_size"].as_i64().unwrap();
    let rows = doc["sweep"].as_array().unwrap();
    assert!(
        rows.len() >= 5,
        "a sweep of {} points is not a sweep",
        rows.len()
    );

    // The constraint: the manifest directory must stay openable at scale.
    const BUDGET: f64 = 4.0;
    let eligible: Vec<&serde_json::Value> = rows
        .iter()
        .filter(|r| r["manifest_bytes_per_row"].as_f64().unwrap() <= BUDGET)
        .collect();
    assert!(!eligible.is_empty());

    // Among the block sizes that fit the budget, the chosen one really does read the
    // fewest bytes.
    let best = eligible
        .iter()
        .min_by_key(|r| r["bytes_read"].as_i64().unwrap())
        .unwrap();
    assert_eq!(
        best["block_size"].as_i64().unwrap(),
        chosen,
        "the evidence chose a block size that is not the one its own rule selects"
    );

    // And the sweep must actually bracket the answer: a winner sitting at the edge of
    // the candidate range has hit a wall, not found an optimum.
    let sizes: Vec<i64> = rows
        .iter()
        .map(|r| r["block_size"].as_i64().unwrap())
        .collect();
    assert!(
        sizes.iter().min().unwrap() < &chosen,
        "the chosen block size is the smallest candidate tested; the sweep did not bracket the \
         optimum and its answer is a boundary artifact, not a measurement"
    );
    assert!(sizes.iter().max().unwrap() > &chosen);
}
