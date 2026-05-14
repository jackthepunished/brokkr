//! Replicated CAS — quorum-write + read-fan-out across N replicas.
//!
//! Phase 3 / M4. Generalises the single-node `Cas` interface to a
//! cluster: every write goes to the `R` replicas the rendezvous-hash
//! ring selects for the blob, and a write succeeds when `⌈R/2⌉ + 1`
//! replicas ack. Reads try replicas in primary-first order and
//! return the first success.
//!
//! ## Architecture
//!
//! M4 ships the *logic*, not the gRPC plumbing. A `ReplicatedCas`
//! is parametrised over an arbitrary `ReplicaPool` — for unit tests
//! we hand it a pool of `InMemoryCas` instances keyed by node id;
//! for production a future milestone will provide a
//! `GrpcReplicaPool` that owns one `ContentAddressableStorageClient`
//! per node and dispatches RPCs over the network. Decoupling the
//! quorum logic from the transport keeps M4's tests deterministic
//! and the replicated layer is the only place where partial-failure
//! handling lives.
//!
//! ## Quorum / failure semantics
//!
//! For a target replication factor `R`:
//!
//! - **Write quorum** is `⌈R/2⌉ + 1`. With `R=2` that's 2 — strict
//!   replicate-to-all. With `R=3` that's 2 — one node can be down
//!   without blocking writes. The plan's §5.4 chose majority over
//!   strict-all because the cold-tier S3 backfill is the
//!   ultimate durability backstop (in a later milestone).
//! - **Reads** try replicas in primary-first order; the first
//!   `Ok` wins. On a miss at every replica, the digest is
//!   genuinely absent.
//! - **`find_missing_blobs`** asks the primary alone — a blob
//!   that's on some replicas and not others still counts as
//!   "present" (write quorum guarantees it'll heal via peer
//!   repair in M5). Phase 3 M4 doesn't issue a parallel
//!   find-missing across replicas; that optimisation is deferred.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use brokkr_common::Digest;
use bytes::Bytes;
use futures::future::join_all;

use crate::error::CasError;
use crate::ring::{replicas_for, RingNode};
use crate::router::Topology;
use crate::traits::{Cas, UpdateResult};

/// Pool of `Cas` backends keyed by their `RingNode.node_id`. In
/// production, the implementation owns a gRPC channel per node and
/// dispatches `ContentAddressableStorage` RPCs over the network.
/// For unit tests, a `HashMapPool<InMemoryCas>` is sufficient.
pub trait ReplicaPool: Send + Sync + 'static {
    /// Look up the `Cas` handle for `node_id`. Returns `None` for
    /// unknown nodes (the caller should surface this as a routing
    /// error).
    fn get(&self, node_id: &str) -> Option<Arc<dyn Cas>>;
}

/// `HashMap<NodeId, Arc<dyn Cas>>` pool. The simplest possible
/// `ReplicaPool` implementation — used in tests and in the
/// in-process control-plane fixture.
#[derive(Default)]
pub struct StaticPool {
    inner: HashMap<String, Arc<dyn Cas>>,
}

impl std::fmt::Debug for StaticPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticPool")
            .field("nodes", &self.inner.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl StaticPool {
    /// Empty pool.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert `cas` keyed by `node_id`. Returns the previous handle
    /// (if any), so the caller can detect accidental clobbers.
    pub fn insert(
        &mut self,
        node_id: impl Into<String>,
        cas: Arc<dyn Cas>,
    ) -> Option<Arc<dyn Cas>> {
        self.inner.insert(node_id.into(), cas)
    }
}

impl ReplicaPool for StaticPool {
    fn get(&self, node_id: &str) -> Option<Arc<dyn Cas>> {
        self.inner.get(node_id).cloned()
    }
}

/// Replicated `Cas` over a `ReplicaPool` and a `Topology` view.
///
/// Each operation consults the topology to pick the responsible
/// replicas, then fans out to the corresponding pool entries.
pub struct ReplicatedCas<P: ReplicaPool> {
    pool: Arc<P>,
    topology: Arc<Topology>,
}

impl<P: ReplicaPool + std::fmt::Debug> std::fmt::Debug for ReplicatedCas<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplicatedCas")
            .field("pool", &self.pool)
            .field(
                "topology",
                &format!(
                    "gen={} R={} nodes={}",
                    self.topology.generation,
                    self.topology.replication_factor,
                    self.topology.nodes.len()
                ),
            )
            .finish()
    }
}

impl<P: ReplicaPool> ReplicatedCas<P> {
    /// Build a replicated CAS bound to a static topology view. A
    /// future milestone will accept a live `Router` so the topology
    /// can change mid-flight.
    pub fn new(pool: Arc<P>, topology: Arc<Topology>) -> Self {
        Self { pool, topology }
    }

    /// Convenience: replicas-for one digest at the configured
    /// replication factor. Returns owned `RingNode` clones.
    fn replicas_for(&self, digest: &Digest) -> Vec<RingNode> {
        let r = self.topology.replication_factor as usize;
        replicas_for(digest, &self.topology.nodes, r)
            .into_iter()
            .cloned()
            .collect()
    }

    fn write_quorum(&self) -> usize {
        let r = self.topology.replication_factor as usize;
        if r == 0 {
            0
        } else {
            r / 2 + 1
        }
    }
}

#[async_trait]
impl<P: ReplicaPool> Cas for ReplicatedCas<P> {
    async fn find_missing_blobs(&self, digests: &[Digest]) -> Result<Vec<Digest>, CasError> {
        // Group by primary replica; ask each one for its slice.
        // We don't fan out across replicas: the write quorum
        // guarantees an eventually-consistent presence answer
        // from any single replica is good enough for the cache
        // miss check. If the primary is down, fall through to
        // the next replica in ring order.
        let mut by_node: HashMap<String, Vec<Digest>> = HashMap::new();
        let mut node_endpoints: HashMap<String, Vec<RingNode>> = HashMap::new();
        for d in digests {
            let replicas = self.replicas_for(d);
            if let Some(primary) = replicas.first() {
                by_node
                    .entry(primary.node_id.clone())
                    .or_default()
                    .push(d.clone());
                node_endpoints
                    .entry(primary.node_id.clone())
                    .or_insert_with(|| replicas.clone());
            } else {
                // No replicas at all (empty topology). The blob
                // is unreachable for now; report missing.
                return Ok(digests.to_vec());
            }
        }

        let mut missing = Vec::new();
        for (node_id, slice) in by_node {
            let replicas = node_endpoints.get(&node_id).cloned().unwrap_or_default();
            let mut answered = false;
            for replica in &replicas {
                if let Some(cas) = self.pool.get(&replica.node_id) {
                    match cas.find_missing_blobs(&slice).await {
                        Ok(m) => {
                            missing.extend(m);
                            answered = true;
                            break;
                        }
                        Err(_) => continue,
                    }
                }
            }
            if !answered {
                // Every replica for this slice was unreachable.
                // Conservative: report the whole slice as missing
                // so the caller re-uploads. (M5 peer repair would
                // catch up later, but in the meantime acting as
                // if the blob is missing is safer than asserting
                // it's present.)
                missing.extend(slice);
            }
        }
        Ok(missing)
    }

    async fn batch_update_blobs(
        &self,
        blobs: Vec<(Digest, Bytes)>,
    ) -> Result<Vec<UpdateResult>, CasError> {
        // For each blob: pick replicas, fan out, count acks. A
        // blob's UpdateResult.status is Ok(()) iff quorum acked.
        let mut out = Vec::with_capacity(blobs.len());
        let quorum = self.write_quorum();
        for (digest, bytes) in blobs {
            let replicas = self.replicas_for(&digest);
            if replicas.is_empty() {
                out.push(UpdateResult {
                    digest,
                    status: Err("no replicas available".to_string()),
                });
                continue;
            }
            // Fan out to every replica in parallel. Each replica
            // independently verifies the digest; we count successes.
            let writes = replicas.iter().filter_map(|r| {
                self.pool.get(&r.node_id).map(|cas| {
                    let d = digest.clone();
                    let b = bytes.clone();
                    async move { cas.batch_update_blobs(vec![(d, b)]).await }
                })
            });
            let results = join_all(writes).await;
            let mut acks = 0usize;
            let mut last_err = None;
            for r in results {
                match r {
                    Ok(per_blob) => {
                        if per_blob.first().map(|x| x.status.is_ok()).unwrap_or(false) {
                            acks += 1;
                        } else if let Some(err) =
                            per_blob.first().and_then(|x| x.status.as_ref().err())
                        {
                            last_err = Some(err.clone());
                        }
                    }
                    Err(e) => last_err = Some(e.to_string()),
                }
            }
            let status = if acks >= quorum {
                Ok(())
            } else {
                Err(last_err
                    .unwrap_or_else(|| format!("write quorum not reached: {acks}/{quorum}")))
            };
            out.push(UpdateResult { digest, status });
        }
        Ok(out)
    }

    async fn batch_read_blobs(
        &self,
        digests: &[Digest],
    ) -> Result<Vec<Result<Bytes, CasError>>, CasError> {
        let mut out = Vec::with_capacity(digests.len());
        for d in digests {
            let replicas = self.replicas_for(d);
            let mut found: Option<Bytes> = None;
            for replica in &replicas {
                let Some(cas) = self.pool.get(&replica.node_id) else {
                    continue;
                };
                match cas.batch_read_blobs(&[d.clone()]).await {
                    Ok(mut results) => match results.pop() {
                        Some(Ok(bytes)) => {
                            found = Some(bytes);
                            break;
                        }
                        Some(Err(_)) | None => continue,
                    },
                    Err(_) => continue,
                }
            }
            out.push(match found {
                Some(bytes) => Ok(bytes),
                None => Err(CasError::NotFound(d.clone())),
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::in_memory::InMemoryCas;
    use crate::ring::NodeStatus;

    fn blob(payload: &[u8]) -> (Digest, Bytes) {
        (Digest::of(payload), Bytes::copy_from_slice(payload))
    }

    fn topology(ids: &[&str], r: u32) -> Topology {
        Topology {
            generation: 1,
            replication_factor: r,
            nodes: ids
                .iter()
                .map(|id| RingNode {
                    node_id: (*id).to_string(),
                    endpoint: format!("http://{id}:7980"),
                    status: NodeStatus::Healthy,
                })
                .collect(),
        }
    }

    fn pool_with(ids: &[&str]) -> (Arc<StaticPool>, Vec<Arc<InMemoryCas>>) {
        let mut pool = StaticPool::new();
        let mut backends = Vec::new();
        for id in ids {
            let cas = Arc::new(InMemoryCas::new());
            backends.push(cas.clone());
            pool.insert(*id, cas);
        }
        (Arc::new(pool), backends)
    }

    #[tokio::test]
    async fn write_fans_out_to_replicas() {
        let (pool, backends) = pool_with(&["a", "b", "c"]);
        let topo = Arc::new(topology(&["a", "b", "c"], 2));
        let cas = ReplicatedCas::new(pool, topo);
        let (d, b) = blob(b"hello");
        let result = cas.batch_update_blobs(vec![(d.clone(), b)]).await.unwrap();
        assert!(result[0].status.is_ok());

        // Exactly 2 of the 3 backends should hold the blob (HRW
        // picks 2 for R=2).
        let holders = backends
            .iter()
            .filter(|c| {
                futures::executor::block_on(c.find_missing_blobs(&[d.clone()]))
                    .map(|m| m.is_empty())
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(holders, 2, "R=2 should have left exactly 2 holders");
    }

    #[tokio::test]
    async fn read_serves_from_first_available_replica() {
        let (pool, _backends) = pool_with(&["a", "b", "c"]);
        let topo = Arc::new(topology(&["a", "b", "c"], 2));
        let cas = ReplicatedCas::new(pool, topo);
        let (d, b) = blob(b"hello world");
        cas.batch_update_blobs(vec![(d.clone(), b.clone())])
            .await
            .unwrap();
        let read = cas.batch_read_blobs(&[d]).await.unwrap();
        assert_eq!(read[0].as_ref().unwrap(), &b);
    }

    #[tokio::test]
    async fn read_returns_not_found_when_no_replica_has_blob() {
        let (pool, _backends) = pool_with(&["a", "b", "c"]);
        let topo = Arc::new(topology(&["a", "b", "c"], 2));
        let cas = ReplicatedCas::new(pool, topo);
        let (d, _) = blob(b"never stored");
        let read = cas.batch_read_blobs(&[d]).await.unwrap();
        assert!(matches!(read[0], Err(CasError::NotFound(_))));
    }

    #[tokio::test]
    async fn write_quorum_means_one_replica_down_still_succeeds_with_r3() {
        // With R=3, quorum is 2. Build a pool with only the first
        // two of the three responsible replicas; the third's
        // lookup returns None — simulating a node being down or
        // unreachable.
        let (writable_pool, backends) = pool_with(&["a", "b", "c"]);
        // Hack: explicitly *remove* the third backend by building
        // a fresh StaticPool that only knows two of the three node ids.
        // Pick a probe digest to find out which node would be third.
        let (d_probe, _) = blob(b"probe");
        let topo = Arc::new(topology(&["a", "b", "c"], 3));
        let three: Vec<RingNode> = replicas_for(&d_probe, &topo.nodes, 3)
            .into_iter()
            .cloned()
            .collect();
        let excluded = three[2].node_id.clone();
        // Build a new StaticPool from the existing one, sans the
        // excluded entry. (StaticPool isn't built for surgical
        // mutation; we rebuild.)
        let _ = writable_pool; // discard the original pool
        let mut tighter = StaticPool::new();
        for (id, cas) in ["a", "b", "c"].iter().zip(backends.iter()) {
            if *id == excluded {
                continue;
            }
            tighter.insert(*id, cas.clone());
        }
        let cas = ReplicatedCas::new(Arc::new(tighter), topo);

        let result = cas
            .batch_update_blobs(vec![(d_probe.clone(), Bytes::from_static(b"probe"))])
            .await
            .unwrap();
        assert!(
            result[0].status.is_ok(),
            "write should succeed at 2/3 quorum even with one replica down: {:?}",
            result[0].status,
        );
    }

    #[tokio::test]
    async fn write_fails_when_quorum_not_reached() {
        // R=2 means quorum=2; if only one replica is reachable,
        // writes fail. Build a pool with only one of the two
        // responsible replicas present.
        let (_old, backends) = pool_with(&["a", "b"]);
        let topo = Arc::new(topology(&["a", "b"], 2));
        let (d_probe, _) = blob(b"probe2");
        let two: Vec<RingNode> = replicas_for(&d_probe, &topo.nodes, 2)
            .into_iter()
            .cloned()
            .collect();
        let included = two[0].node_id.clone();

        let mut tighter = StaticPool::new();
        for (id, cas) in ["a", "b"].iter().zip(backends.iter()) {
            if *id == included {
                tighter.insert(*id, cas.clone());
            }
        }
        let cas = ReplicatedCas::new(Arc::new(tighter), topo);

        let result = cas
            .batch_update_blobs(vec![(d_probe.clone(), Bytes::from_static(b"probe2"))])
            .await
            .unwrap();
        assert!(
            result[0].status.is_err(),
            "write must fail when only 1/2 replicas reachable",
        );
    }

    #[tokio::test]
    async fn find_missing_returns_authoritative_answer_from_primary() {
        let (pool, _) = pool_with(&["a", "b", "c"]);
        let topo = Arc::new(topology(&["a", "b", "c"], 2));
        let cas = ReplicatedCas::new(pool, topo);
        let (d1, b1) = blob(b"present");
        let (d2, _) = blob(b"absent");
        cas.batch_update_blobs(vec![(d1.clone(), b1)])
            .await
            .unwrap();
        let missing = cas.find_missing_blobs(&[d1, d2.clone()]).await.unwrap();
        assert_eq!(missing, vec![d2]);
    }

    #[tokio::test]
    async fn empty_topology_reports_everything_missing() {
        let pool = Arc::new(StaticPool::new());
        let topo = Arc::new(topology(&[], 2));
        let cas = ReplicatedCas::new(pool, topo);
        let (d, _) = blob(b"orphan");
        let missing = cas.find_missing_blobs(&[d.clone()]).await.unwrap();
        assert_eq!(missing, vec![d]);
    }
}
