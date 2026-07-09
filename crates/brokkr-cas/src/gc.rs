//! Reference-counted GC for the Phase 3 CAS.
//!
//! Phase 3 / M5a. The control plane is the source of truth for
//! reachability via the action cache: any digest referenced by a
//! live `ActionResult` is reachable; anything else is a candidate
//! for eviction. M5a ships the non-transitive variant — we extract
//! the digests inlined directly in `ActionResult`
//! (`output_files[].digest`, `stdout_digest`, `stderr_digest`,
//! `output_directories[].tree_digest`, etc.). A future milestone
//! will walk `Directory` Merkle DAGs to mark every transitively
//! reachable file too; that walk requires CAS reads and is the
//! kind of slow background work that wants its own scheduling.
//!
//! ## Algorithm
//!
//! ```text
//! reachable = ⋃ direct_digests(ar) for each ar in action_cache
//! unreachable = local_digests - reachable
//! for each d in unreachable: delete d
//! ```
//!
//! ## Retention window
//!
//! The plan calls for a "retention window" so a blob that was
//! unreachable for less than N days is preserved. That requires
//! atime tracking, which Phase 3 M5a does not have. M5b will add
//! atime + the retention filter; M5a's `sweep` deletes any
//! unreachable blob immediately. Callers that want to dry-run
//! should use [`plan`] first and decide whether to call
//! [`sweep_with_plan`].

use std::collections::HashSet;

use brokkr_common::Digest;
use brokkr_proto::reapi_v2 as rapi;

use crate::action_cache::ActionCache;
use crate::error::CasError;
use crate::traits::Cas;

/// Extract every digest directly inlined in an `ActionResult`. Does
/// not transitively expand `Directory` protos; M5b will add the
/// recursive walk.
pub fn direct_digests(ar: &rapi::ActionResult) -> Vec<Digest> {
    let mut out = Vec::new();
    for f in &ar.output_files {
        if let Some(d) = &f.digest {
            if let Ok(d) = Digest::new(d.hash.clone(), d.size_bytes) {
                out.push(d);
            }
        }
    }
    // REAPI v2.1+ deprecates output_file_symlinks in favour of
    // output_symlinks; either way symlinks are paths, not digests.
    for dir in &ar.output_directories {
        if let Some(d) = &dir.tree_digest {
            if let Ok(d) = Digest::new(d.hash.clone(), d.size_bytes) {
                out.push(d);
            }
        }
        if let Some(d) = &dir.root_directory_digest {
            if let Ok(d) = Digest::new(d.hash.clone(), d.size_bytes) {
                out.push(d);
            }
        }
    }
    if let Some(d) = &ar.stdout_digest {
        if let Ok(d) = Digest::new(d.hash.clone(), d.size_bytes) {
            out.push(d);
        }
    }
    if let Some(d) = &ar.stderr_digest {
        if let Ok(d) = Digest::new(d.hash.clone(), d.size_bytes) {
            out.push(d);
        }
    }
    out
}

/// Snapshot of what a GC pass would do without actually deleting
/// anything. Useful for dry-runs and metrics. The
/// `unreachable_digests` set is the candidate-deletion list.
#[derive(Debug, Clone)]
pub struct GcPlan {
    /// Digests reachable from the current action-cache contents.
    pub reachable: HashSet<Digest>,
    /// Digests held locally that are *not* reachable. M5a deletes
    /// every entry in this set; M5b will additionally filter by
    /// atime against the retention window.
    pub unreachable: Vec<Digest>,
    /// Total local digest count, for /metrics.
    pub local_count: usize,
}

/// Compute a [`GcPlan`] without modifying any state. Pure read.
pub async fn plan(cas: &dyn Cas, action_cache: &dyn ActionCache) -> Result<GcPlan, CasError> {
    let entries = action_cache.list_entries().await?;
    let mut reachable: HashSet<Digest> = HashSet::new();
    for (_action_digest, ar) in entries {
        // We do NOT add the action digest itself to the reachable
        // set. The action_cache lookup is keyed on the *hash* of
        // the Action proto, but the proto's encoded length isn't
        // recoverable from the action-cache store (the redb table
        // only carries the hash hex). Action / Command /
        // Directory protos are inputs; clients re-upload them via
        // `FindMissingBlobs` on the next request, so GC'ing them
        // is a perf hit, not a correctness violation. Keeping
        // them is M5b territory once we track per-entry size on
        // disk.
        for d in direct_digests(&ar) {
            reachable.insert(d);
        }
    }

    let local = cas.list_digests().await?;
    let local_count = local.len();
    let unreachable: Vec<Digest> = local
        .into_iter()
        .filter(|d| !reachable.contains(d))
        .collect();

    Ok(GcPlan {
        reachable,
        unreachable,
        local_count,
    })
}

/// Run a GC sweep: compute the plan, delete every blob in
/// `unreachable`. Returns the count of deleted blobs.
pub async fn sweep(cas: &dyn Cas, action_cache: &dyn ActionCache) -> Result<usize, CasError> {
    let plan = plan(cas, action_cache).await?;
    sweep_with_plan(cas, &plan).await
}

/// Execute deletions from a pre-computed plan. Useful when a
/// caller wants to dry-run [`plan`] first or apply a custom
/// retention filter between planning and sweeping.
pub async fn sweep_with_plan(cas: &dyn Cas, plan: &GcPlan) -> Result<usize, CasError> {
    let mut deleted = 0usize;
    for d in &plan.unreachable {
        cas.delete_blob(d).await?;
        deleted += 1;
    }
    Ok(deleted)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods)]
mod tests {
    use std::sync::Arc;

    use brokkr_proto::reapi_v2 as rapi;
    use bytes::Bytes;

    use super::*;
    use crate::action_cache::{ActionCache, RedbActionCache};
    use crate::in_memory::InMemoryCas;
    use crate::traits::Cas;

    fn blob(payload: &[u8]) -> (Digest, Bytes) {
        (Digest::of(payload), Bytes::copy_from_slice(payload))
    }

    fn proto_digest(d: &Digest) -> rapi::Digest {
        rapi::Digest {
            hash: d.hash().to_string(),
            size_bytes: d.size_bytes(),
        }
    }

    fn ar_with_outputs(outputs: &[&Digest]) -> rapi::ActionResult {
        rapi::ActionResult {
            output_files: outputs
                .iter()
                .map(|d| rapi::OutputFile {
                    path: "out".into(),
                    digest: Some(proto_digest(d)),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn direct_digests_extracts_output_and_streams() {
        let out = Digest::of(b"output");
        let stdout = Digest::of(b"stdout");
        let stderr = Digest::of(b"stderr");
        let ar = rapi::ActionResult {
            output_files: vec![rapi::OutputFile {
                path: "f".into(),
                digest: Some(proto_digest(&out)),
                ..Default::default()
            }],
            stdout_digest: Some(proto_digest(&stdout)),
            stderr_digest: Some(proto_digest(&stderr)),
            ..Default::default()
        };
        let found: HashSet<Digest> = direct_digests(&ar).into_iter().collect();
        assert_eq!(found.len(), 3);
        assert!(found.contains(&out));
        assert!(found.contains(&stdout));
        assert!(found.contains(&stderr));
    }

    #[test]
    fn direct_digests_skips_malformed_proto() {
        // A malformed Digest (wrong hash length) is silently
        // skipped, not propagated as an error. GC should be
        // resilient to historical bad data.
        let ar = rapi::ActionResult {
            output_files: vec![rapi::OutputFile {
                path: "f".into(),
                digest: Some(rapi::Digest {
                    hash: "not-a-hex-digest".into(),
                    size_bytes: 0,
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(direct_digests(&ar).is_empty());
    }

    #[tokio::test]
    async fn plan_marks_reachable_correctly() {
        let cas = InMemoryCas::new();
        let dir = tempfile::tempdir().unwrap();
        let ac = RedbActionCache::open(dir.path().join("ac.redb")).unwrap();

        // Set up: two output blobs, one cached, one orphaned.
        let (out_cached, _) = blob(b"cached output");
        let (out_orphan, _) = blob(b"orphan output");
        cas.batch_update_blobs(vec![
            (out_cached.clone(), Bytes::from_static(b"cached output")),
            (out_orphan.clone(), Bytes::from_static(b"orphan output")),
        ])
        .await
        .unwrap();

        // Register an ActionResult that references only `out_cached`.
        let action_digest = Digest::of(b"action-1");
        ac.update_action_result(&action_digest, ar_with_outputs(&[&out_cached]))
            .await
            .unwrap();

        let plan = plan(&cas, &ac).await.unwrap();
        assert!(plan.reachable.contains(&out_cached));
        // Action digests are not part of the reachable set —
        // see the `plan` doc-comment for why.
        let _ = action_digest;
        assert!(!plan.reachable.contains(&out_orphan));
        assert_eq!(plan.unreachable, vec![out_orphan.clone()]);
        assert_eq!(plan.local_count, 2);
    }

    #[tokio::test]
    async fn sweep_deletes_unreachable_blobs() {
        let cas = Arc::new(InMemoryCas::new());
        let dir = tempfile::tempdir().unwrap();
        let ac = RedbActionCache::open(dir.path().join("ac.redb")).unwrap();
        let (live, _) = blob(b"live");
        let (dead, _) = blob(b"dead");
        cas.batch_update_blobs(vec![
            (live.clone(), Bytes::from_static(b"live")),
            (dead.clone(), Bytes::from_static(b"dead")),
        ])
        .await
        .unwrap();
        ac.update_action_result(&Digest::of(b"action"), ar_with_outputs(&[&live]))
            .await
            .unwrap();

        let deleted = sweep(cas.as_ref(), &ac).await.unwrap();
        assert_eq!(deleted, 1);

        let after = cas.list_digests().await.unwrap();
        assert_eq!(after, vec![live]);
    }

    #[tokio::test]
    async fn empty_action_cache_marks_everything_unreachable() {
        let cas = InMemoryCas::new();
        let dir = tempfile::tempdir().unwrap();
        let ac = RedbActionCache::open(dir.path().join("ac.redb")).unwrap();
        let (b1, _) = blob(b"one");
        let (b2, _) = blob(b"two");
        cas.batch_update_blobs(vec![
            (b1.clone(), Bytes::from_static(b"one")),
            (b2.clone(), Bytes::from_static(b"two")),
        ])
        .await
        .unwrap();
        let plan = plan(&cas, &ac).await.unwrap();
        assert!(plan.reachable.is_empty());
        assert_eq!(plan.unreachable.len(), 2);
    }

    #[tokio::test]
    async fn sweep_is_idempotent() {
        // Running GC twice in a row deletes the same set once
        // and no-ops the second time.
        let cas = InMemoryCas::new();
        let dir = tempfile::tempdir().unwrap();
        let ac = RedbActionCache::open(dir.path().join("ac.redb")).unwrap();
        let (live, _) = blob(b"live");
        let (dead, _) = blob(b"dead");
        cas.batch_update_blobs(vec![
            (live.clone(), Bytes::from_static(b"live")),
            (dead.clone(), Bytes::from_static(b"dead")),
        ])
        .await
        .unwrap();
        ac.update_action_result(&Digest::of(b"a"), ar_with_outputs(&[&live]))
            .await
            .unwrap();
        assert_eq!(sweep(&cas, &ac).await.unwrap(), 1);
        assert_eq!(sweep(&cas, &ac).await.unwrap(), 0);
    }

    // --- #143 cross-decorator regression tests ---
    //
    // These run `sweep` end-to-end through each of the three
    // decorator types. Pre-fix, the trait default no-opped and
    // every sweep returned 0 while leaving data on disk; these
    // tests would have failed at the `sweep returns 1` line.

    #[tokio::test]
    async fn sweep_works_through_tiered_decorator() {
        use crate::tiered::TieredCas;
        use std::sync::Arc;

        let warm = Arc::new(InMemoryCas::new());
        let cas = TieredCas::new(warm.clone(), 1024);
        let dir = tempfile::tempdir().unwrap();
        let ac = RedbActionCache::open(dir.path().join("ac.redb")).unwrap();
        let (live, _) = blob(b"live-tiered");
        let (dead, _) = blob(b"dead-tiered");
        cas.batch_update_blobs(vec![
            (live.clone(), Bytes::from_static(b"live-tiered")),
            (dead.clone(), Bytes::from_static(b"dead-tiered")),
        ])
        .await
        .unwrap();
        ac.update_action_result(&Digest::of(b"a"), ar_with_outputs(&[&live]))
            .await
            .unwrap();

        assert_eq!(sweep(&cas, &ac).await.unwrap(), 1);

        // The dead blob is gone from warm.
        let read = warm.batch_read_blobs(&[dead.clone()]).await.unwrap();
        assert!(matches!(read[0], Err(CasError::NotFound(_))));
        // The live blob survives.
        let live_read = warm.batch_read_blobs(&[live]).await.unwrap();
        assert!(live_read[0].is_ok());
    }

    #[tokio::test]
    async fn sweep_works_through_bloom_decorator() {
        use crate::bloom_cas::BloomCas;
        use std::sync::Arc;

        let inner = Arc::new(InMemoryCas::new());
        let cas = BloomCas::new(inner.clone(), 1024, 0.01);
        let dir = tempfile::tempdir().unwrap();
        let ac = RedbActionCache::open(dir.path().join("ac.redb")).unwrap();
        let (live, _) = blob(b"live-bloom");
        let (dead, _) = blob(b"dead-bloom");
        cas.batch_update_blobs(vec![
            (live.clone(), Bytes::from_static(b"live-bloom")),
            (dead.clone(), Bytes::from_static(b"dead-bloom")),
        ])
        .await
        .unwrap();
        ac.update_action_result(&Digest::of(b"a"), ar_with_outputs(&[&live]))
            .await
            .unwrap();

        assert_eq!(sweep(&cas, &ac).await.unwrap(), 1);

        // The dead blob is gone from the inner store.
        let read = inner.batch_read_blobs(&[dead.clone()]).await.unwrap();
        assert!(matches!(read[0], Err(CasError::NotFound(_))));
    }

    #[tokio::test]
    async fn sweep_works_through_replicated_decorator() {
        use crate::replicated::{ReplicatedCas, StaticPool};
        use crate::ring::NodeStatus;
        use std::sync::Arc;

        // Build a 3-node cluster with R=2.
        let mut pool = StaticPool::new();
        let backends: Vec<Arc<InMemoryCas>> =
            (0..3).map(|_| Arc::new(InMemoryCas::new())).collect();
        for (i, c) in backends.iter().enumerate() {
            pool.insert(format!("n{i}"), c.clone() as Arc<dyn crate::traits::Cas>);
        }
        let topo = Arc::new(crate::Topology {
            generation: 1,
            replication_factor: 2,
            nodes: (0..3)
                .map(|i| crate::ring::RingNode {
                    node_id: format!("n{i}"),
                    endpoint: format!("http://n{i}:7980"),
                    status: NodeStatus::Healthy,
                })
                .collect(),
        });
        let cas = ReplicatedCas::new(Arc::new(pool), topo);

        let dir = tempfile::tempdir().unwrap();
        let ac = RedbActionCache::open(dir.path().join("ac.redb")).unwrap();
        let (live, _) = blob(b"live-repl");
        let (dead, _) = blob(b"dead-repl");
        cas.batch_update_blobs(vec![
            (live.clone(), Bytes::from_static(b"live-repl")),
            (dead.clone(), Bytes::from_static(b"dead-repl")),
        ])
        .await
        .unwrap();
        ac.update_action_result(&Digest::of(b"a"), ar_with_outputs(&[&live]))
            .await
            .unwrap();

        // sweep reports 1 (one unreachable digest), even though
        // the underlying fan-out deletes hit R=2 replicas.
        assert_eq!(sweep(&cas, &ac).await.unwrap(), 1);

        // Every replica the ring selected for `dead` is now
        // missing it.
        let holders_with_dead: Vec<usize> = backends
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                futures::executor::block_on(c.find_missing_blobs(&[dead.clone()]))
                    .map(|m| m.is_empty())
                    .unwrap_or(false)
            })
            .map(|(i, _)| i)
            .collect();
        assert!(
            holders_with_dead.is_empty(),
            "replicas still hold the deleted blob: {holders_with_dead:?}"
        );
    }
}
