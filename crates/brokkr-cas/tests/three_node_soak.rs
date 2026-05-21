//! Phase 3 M7 — three-node soak.
//!
//! Drives a `ReplicatedCas` (R=2) over a 3-node in-process pool
//! through a randomized mix of `put` / `get` / `find_missing_blobs`
//! while a background "node-churn" loop periodically swaps one
//! non-primary replica for a fresh empty `InMemoryCas` and waits
//! for `repair_node` to converge. At the end of the soak the test
//! checks the four §7.3.1 invariants:
//!
//! 1. **No data loss** — every put digest still reads via the
//!    replicated CAS and returns the original bytes.
//! 2. **No orphans** — `repair_cluster` after the run reports zero
//!    new repairs and zero unrepairable blobs.
//! 3. **Quiescence** — that final `repair_cluster` returns in
//!    < 1 s.
//! 4. **Bounded per-node count** — each node's `list_digests` is
//!    exactly the subset of `live` digests that HRW assigns to
//!    that node.
//!
//! `#[ignore]` by default so `cargo test --workspace` stays fast.
//! Tune via env vars (printed at the start of every run):
//!
//! | Env var | Default | Plan-§11 release-gate value |
//! |---|---|---|
//! | `BROKKR_SOAK_OPS` | `25000` | `1000000` |
//! | `BROKKR_SOAK_CHURN` | `250` | `2000` |
//! | `BROKKR_SOAK_SEED` | random | a fixed seed for reproduction |
//!
//! Run locally:
//!
//! ```text
//! cargo test -p brokkr-cas --test three_node_soak -- --ignored
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_methods,
    clippy::panic
)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use brokkr_cas::traits::UpdateResult;
use brokkr_cas::{
    repair_cluster, repair_node, replicas_for, Cas, InMemoryCas, NodeStatus, ReplicaPool,
    ReplicatedCas, RingNode, Topology,
};
use brokkr_common::Digest;
use bytes::Bytes;
use parking_lot::RwLock;
use rand::rngs::StdRng;
use rand::{Rng, RngCore, SeedableRng};

const NODE_IDS: [&str; 3] = ["cas-alpha", "cas-beta", "cas-gamma"];
const REPLICATION_FACTOR: usize = 2;

/// Replica pool whose node bindings can be swapped at runtime, so
/// the churn loop can simulate "node restart with empty disk"
/// without rebuilding the whole `ReplicatedCas`.
#[derive(Default)]
struct MutablePool {
    inner: RwLock<HashMap<String, Arc<dyn Cas>>>,
}

impl std::fmt::Debug for MutablePool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MutablePool")
            .field("nodes", &self.inner.read().keys().collect::<Vec<_>>())
            .finish()
    }
}

impl MutablePool {
    fn new(nodes: &[(&str, Arc<dyn Cas>)]) -> Arc<Self> {
        let mut map = HashMap::new();
        for (id, cas) in nodes {
            map.insert((*id).to_string(), cas.clone());
        }
        Arc::new(Self {
            inner: RwLock::new(map),
        })
    }

    /// Replace the CAS handle for `node_id`. Returns the previous
    /// handle so the caller can drop it (and its data).
    fn replace(&self, node_id: &str, cas: Arc<dyn Cas>) -> Option<Arc<dyn Cas>> {
        self.inner.write().insert(node_id.to_string(), cas)
    }
}

impl ReplicaPool for MutablePool {
    fn get(&self, node_id: &str) -> Option<Arc<dyn Cas>> {
        self.inner.read().get(node_id).cloned()
    }
}

fn make_topology() -> Topology {
    Topology {
        generation: 1,
        nodes: NODE_IDS
            .iter()
            .map(|id| RingNode {
                node_id: (*id).to_string(),
                endpoint: format!("in-process://{id}"),
                status: NodeStatus::Healthy,
            })
            .collect(),
        replication_factor: REPLICATION_FACTOR as u32,
    }
}

fn parse_env_usize(var: &str, default: usize) -> usize {
    match std::env::var(var) {
        Ok(v) => v.parse().unwrap_or_else(|e| {
            eprintln!("{var}={v:?} is not a usize ({e}); using default {default}");
            default
        }),
        Err(_) => default,
    }
}

fn parse_env_seed() -> u64 {
    match std::env::var("BROKKR_SOAK_SEED") {
        Ok(v) => v.parse().unwrap_or_else(|_| rand::thread_rng().next_u64()),
        Err(_) => rand::thread_rng().next_u64(),
    }
}

/// One operation in the soak's mix. Weights match §7.3.1.
#[derive(Debug, Clone, Copy)]
enum Op {
    Put,
    Get,
    FindMissing,
}

fn pick_op(rng: &mut StdRng) -> Op {
    let r: f64 = rng.gen();
    if r < 0.45 {
        Op::Put
    } else if r < 0.90 {
        Op::Get
    } else {
        Op::FindMissing
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "soak test; run with --ignored"]
async fn three_node_soak() {
    let ops_budget = parse_env_usize("BROKKR_SOAK_OPS", 25_000);
    let churn_every = parse_env_usize("BROKKR_SOAK_CHURN", 250);
    let seed = parse_env_seed();
    let start = Instant::now();

    eprintln!(
        "[soak] ops={ops_budget} churn_every={churn_every} seed={seed} \
         replication_factor={REPLICATION_FACTOR}"
    );

    let mut rng = StdRng::seed_from_u64(seed);

    // Stand up three in-memory replicas behind a MutablePool.
    let nodes_arc: Vec<(&str, Arc<dyn Cas>)> = NODE_IDS
        .iter()
        .map(|id| (*id, Arc::new(InMemoryCas::new()) as Arc<dyn Cas>))
        .collect();
    let pool = MutablePool::new(&nodes_arc);
    let topology = Arc::new(make_topology());
    let cas = ReplicatedCas::new(pool.clone(), topology.clone());

    // Ground truth: every digest we successfully `put` and the
    // bytes we put under it.
    let mut live: HashMap<Digest, Bytes> = HashMap::new();
    let mut churns = 0usize;
    let mut puts = 0usize;
    let mut gets = 0usize;
    let mut find_missings = 0usize;

    for op_idx in 0..ops_budget {
        match pick_op(&mut rng) {
            Op::Put => {
                let size = rng.gen_range(64..=1024);
                let mut buf = vec![0u8; size];
                rng.fill_bytes(&mut buf);
                let bytes = Bytes::from(buf);
                let digest = Digest::of(&bytes);
                let results = cas
                    .batch_update_blobs(vec![(digest.clone(), bytes.clone())])
                    .await
                    .unwrap_or_else(|e| panic!("put failed at op {op_idx} (seed={seed}): {e:?}"));
                assert_eq!(results.len(), 1);
                assert!(
                    matches!(&results[0], UpdateResult { status: Ok(()), .. }),
                    "put rejected at op {op_idx} (seed={seed}): {results:?}"
                );
                live.insert(digest, bytes);
                puts += 1;
            }
            Op::Get => {
                if live.is_empty() {
                    continue;
                }
                let keys: Vec<&Digest> = live.keys().collect();
                let pick = keys[rng.gen_range(0..keys.len())].clone();
                let expected = live[&pick].clone();
                let results = cas
                    .batch_read_blobs(&[pick.clone()])
                    .await
                    .unwrap_or_else(|e| panic!("read failed at op {op_idx} (seed={seed}): {e:?}"));
                assert_eq!(results.len(), 1);
                let got = results.into_iter().next().unwrap().unwrap_or_else(|e| {
                    panic!("read missing at op {op_idx} digest={pick:?} (seed={seed}): {e:?}")
                });
                assert_eq!(
                    got, expected,
                    "byte mismatch at op {op_idx} digest={pick:?} (seed={seed})"
                );
                gets += 1;
            }
            Op::FindMissing => {
                // Mix: a few live digests (should NOT appear in
                // result) + a few fabricated ones (should appear).
                let mut probes: Vec<Digest> = Vec::new();
                let mut expected_missing: HashSet<Digest> = HashSet::new();
                if !live.is_empty() {
                    let keys: Vec<&Digest> = live.keys().collect();
                    for _ in 0..3.min(keys.len()) {
                        probes.push(keys[rng.gen_range(0..keys.len())].clone());
                    }
                }
                for _ in 0..2 {
                    let mut buf = [0u8; 32];
                    rng.fill_bytes(&mut buf);
                    let d = Digest::of(&buf);
                    probes.push(d.clone());
                    expected_missing.insert(d);
                }
                let actual: HashSet<Digest> = cas
                    .find_missing_blobs(&probes)
                    .await
                    .unwrap_or_else(|e| {
                        panic!("find_missing failed at op {op_idx} (seed={seed}): {e:?}")
                    })
                    .into_iter()
                    .collect();
                assert_eq!(
                    actual, expected_missing,
                    "find_missing diverged at op {op_idx} (seed={seed})"
                );
                find_missings += 1;
            }
        }

        // Churn: every `churn_every` ops, swap a random node for an
        // empty CAS and let peer-repair converge.
        if churn_every > 0 && op_idx > 0 && op_idx % churn_every == 0 {
            let victim_idx = rng.gen_range(0..NODE_IDS.len());
            let victim = NODE_IDS[victim_idx];
            let fresh: Arc<dyn Cas> = Arc::new(InMemoryCas::new());
            pool.replace(victim, fresh);
            // Repair immediately. We're enforcing the §7.3.1 "one
            // node at a time" invariant by not starting another
            // churn until repair_node returns.
            let report = repair_node(&*pool, &topology, victim)
                .await
                .unwrap_or_else(|e| {
                    panic!("repair_node({victim}) failed at op {op_idx} (seed={seed}): {e:?}")
                });
            // Repair must converge: no unrepairable blobs.
            assert!(
                report.unrepairable.is_empty(),
                "repair_node({victim}) reported {} unrepairable blobs at op {op_idx} (seed={seed}): {:?}",
                report.unrepairable.len(),
                report.unrepairable,
            );
            churns += 1;
        }
    }

    let soak_elapsed = start.elapsed();
    eprintln!(
        "[soak] loop done in {:?}: puts={puts} gets={gets} find_missings={find_missings} \
         churns={churns} live={}",
        soak_elapsed,
        live.len(),
    );

    // ---- Invariant 1: no data loss ----
    let all_digests: Vec<Digest> = live.keys().cloned().collect();
    let reads = cas
        .batch_read_blobs(&all_digests)
        .await
        .unwrap_or_else(|e| panic!("final readback failed (seed={seed}): {e:?}"));
    assert_eq!(reads.len(), all_digests.len());
    for (digest, result) in all_digests.iter().zip(reads.into_iter()) {
        let bytes = result.unwrap_or_else(|e| {
            panic!("final readback: missing digest {digest:?} (seed={seed}): {e:?}")
        });
        assert_eq!(
            bytes, live[digest],
            "final readback: byte mismatch for {digest:?} (seed={seed})"
        );
    }

    // ---- Invariants 2 & 3: no orphans + quiescence ----
    let quiesce_start = Instant::now();
    let cluster = repair_cluster(&*pool, &topology)
        .await
        .unwrap_or_else(|e| panic!("final repair_cluster failed (seed={seed}): {e:?}"));
    let quiesce_elapsed = quiesce_start.elapsed();
    let total_repairs: usize = cluster.iter().map(|(_, r)| r.repaired).sum();
    let total_unrepairable: usize = cluster.iter().map(|(_, r)| r.unrepairable.len()).sum();
    assert_eq!(
        total_unrepairable, 0,
        "final repair_cluster found unrepairable blobs (seed={seed}): {cluster:#?}"
    );
    assert_eq!(
        total_repairs, 0,
        "final repair_cluster still found work to do — peer-repair didn't converge during the soak (seed={seed}): {cluster:#?}"
    );
    assert!(
        quiesce_elapsed < Duration::from_secs(1),
        "final repair_cluster took {quiesce_elapsed:?}, > 1 s (seed={seed})"
    );

    // ---- Invariant 4: bounded per-node count ----
    // For each node, compute the subset of `live` that HRW assigns
    // to it under R=2, then compare against the node's
    // `list_digests`.
    let topo_nodes = topology.nodes.clone();
    for node in &topo_nodes {
        let expected: HashSet<Digest> = live
            .keys()
            .filter(|d| {
                replicas_for(d, &topo_nodes, REPLICATION_FACTOR)
                    .iter()
                    .any(|rn| rn.node_id == node.node_id)
            })
            .cloned()
            .collect();
        let actual: HashSet<Digest> = pool
            .get(&node.node_id)
            .unwrap_or_else(|| panic!("pool missing {} after soak", node.node_id))
            .list_digests()
            .await
            .unwrap_or_else(|e| panic!("list_digests({}) failed: {e:?}", node.node_id))
            .into_iter()
            .collect();
        assert_eq!(
            actual,
            expected,
            "node {} held a different blob set than HRW assignment (seed={seed}): \
             missing-on-node={:?} extra-on-node={:?}",
            node.node_id,
            expected.difference(&actual).count(),
            actual.difference(&expected).count(),
        );
    }

    eprintln!(
        "[soak] PASS — total elapsed {:?}, quiesce {:?}, seed={seed}",
        start.elapsed(),
        quiesce_elapsed
    );
}
