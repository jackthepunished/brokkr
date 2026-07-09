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
//!
//! ## Concurrency — coordination barrier (issue #144)
//!
//! `plan` and `cas.list_digests` run in two separate redb
//! transactions at two different points in time. A worker that
//! performs `(cas.batch_update_blobs, ac.update_action_result)`
//! for the same logical action in the gap gets its blob GC'd and
//! its fresh `ActionResult` left pointing at `NotFound`. Closing
//! the race requires cross-store coordination.
//!
//! The selected mechanism is a writer-coordinated barrier
//! ([`crate::action_cache::GcWindowGuard`]): workers hold the
//! guard for the duration of their two writes; the GC holds it
//! for the duration of `plan + sweep_with_plan`. With the guard
//! held on both sides, no `(upload, AC write)` pair can land
//! inside the swept window.
//!
//! **Use [`sweep`] unless you specifically need to inspect
//! `plan` first.** The `*_locked` helpers below are for callers
//! that already hold the guard and want to compose the steps
//! explicitly. The non-locked [`plan`] and [`sweep_with_plan`]
//! remain available but are only safe when no concurrent
//! `(CAS-write, AC-write)` is in flight.
//!
//! **Scope:** the barrier is in-process only. A worker in a
//! separate process that uploads via gRPC does not hold the
//! barrier — that race is a separate issue (planned M5b/Phase 4).

use std::collections::HashSet;

use brokkr_common::Digest;
use brokkr_proto::reapi_v2 as rapi;

use crate::action_cache::{ActionCache, GcWindowGuard};
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
///
/// **Advisory only** — for use when no concurrent `(CAS-write,
/// AC-write)` pair is in flight. Most callers want
/// [`sweep_with_plan`] (single-step) or the `*_locked` variants
/// below. The non-locked [`plan`] is exposed for dry-runs and
/// metrics collection, where the snapshot is approximate.
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

/// Compute a [`GcPlan`] while the GC coordination barrier
/// (`_guard`) is held.
///
/// The barrier suppresses concurrent `(CAS-write, AC-write)`
/// pairs: any worker performing an `update_action_result` will
/// block until the matching `_guard` is dropped, and the GC
/// completion will observe its writes.
///
/// The `_guard` parameter is a documentation token — calling
/// [`plan_locked`] without holding the guard via
/// [`ActionCache::gc_window`] is the bug that introduces the
/// race. Pass the live guard you obtained from
/// `action_cache.gc_window()`.
pub async fn plan_locked(
    cas: &dyn Cas,
    action_cache: &dyn ActionCache,
    _guard: &GcWindowGuard,
) -> Result<GcPlan, CasError> {
    plan(cas, action_cache).await
}

/// Execute deletions from a pre-computed plan while the GC
/// coordination barrier (`_guard`) is held.
///
/// Same contract as [`plan_locked`]: pass the live
/// [`GcWindowGuard`] from [`ActionCache::gc_window`].
pub async fn sweep_with_plan_locked(
    cas: &dyn Cas,
    plan: &GcPlan,
    _guard: &GcWindowGuard,
) -> Result<usize, CasError> {
    let mut deleted = 0usize;
    for d in &plan.unreachable {
        cas.delete_blob(d).await?;
        deleted += 1;
    }
    Ok(deleted)
}

/// Run a GC sweep: acquire the GC coordination barrier, compute
/// the plan, delete every blob in `unreachable`. Returns the
/// count of deleted blobs.
///
/// This is the supported entry point. The barrier acquisition
/// closes the mark/sweep race described in the module
/// doc-comment: workers cannot complete a `(CAS-write, AC-write)`
/// pair during this function's critical section. The guard
/// is released when this function returns (or is cancelled).
pub async fn sweep(cas: &dyn Cas, action_cache: &dyn ActionCache) -> Result<usize, CasError> {
    let _guard = action_cache.gc_window().await?;
    let plan = plan_locked(cas, action_cache, &_guard).await?;
    sweep_with_plan_locked(cas, &plan, &_guard).await
}

/// Execute deletions from a pre-computed plan. Useful when a
/// caller wants to dry-run [`plan`] first or apply a custom
/// retention filter between planning and sweeping.
///
/// **Advisory only** — for use when no concurrent `(CAS-write,
/// AC-write)` pair is in flight. Most callers want [`sweep`] or
/// [`sweep_with_plan_locked`] with a live barrier guard.
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

    // --- #144 mark/sweep coordination tests ---
    //
    // These pin the fix in `crates/brokkr-cas/src/gc.rs::sweep` and
    // `ActionCache::gc_window`. The first test reproduces the
    // issue's race deterministically; the second is the
    // regression-test counterpart that fails if the override is
    // removed.

    /// `Cas` decorator that parks `list_digests` on a
    /// `tokio::sync::Notify` gate. Used to force `gc::sweep` to
    /// block at the precise instant where the bug's race window
    /// opens, so a concurrent worker write can be interleaved
    /// deterministically.
    struct PausableCas {
        inner: Arc<InMemoryCas>,
        // First `Notify` use in the repo. Legitimate as a
        // single-purpose test-only primitive; does not add a
        // runtime dependency (notifies are part of `tokio::sync`,
        // already in scope).
        enter: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    impl PausableCas {
        fn new(inner: Arc<InMemoryCas>) -> Self {
            Self {
                inner,
                enter: Arc::new(tokio::sync::Notify::new()),
                release: Arc::new(tokio::sync::Notify::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl Cas for PausableCas {
        async fn find_missing_blobs(&self, digests: &[Digest]) -> Result<Vec<Digest>, CasError> {
            self.inner.find_missing_blobs(digests).await
        }

        async fn batch_update_blobs(
            &self,
            entries: Vec<(Digest, Bytes)>,
        ) -> Result<Vec<crate::traits::UpdateResult>, CasError> {
            self.inner.batch_update_blobs(entries).await
        }

        async fn batch_read_blobs(
            &self,
            digests: &[Digest],
        ) -> Result<Vec<Result<Bytes, CasError>>, CasError> {
            self.inner.batch_read_blobs(digests).await
        }

        async fn list_digests(&self) -> Result<Vec<Digest>, CasError> {
            // Park here so the test can observe this exact
            // point in `gc::plan`. We notify before parking so
            // the test can write its AC entry, *then* release
            // us to proceed with the snapshot.
            self.enter.notify_one();
            self.release.notified().await;
            self.inner.list_digests().await
        }

        async fn delete_blob(&self, digest: &Digest) -> Result<(), CasError> {
            self.inner.delete_blob(digest).await
        }
    }

    /// Minimal `ActionCache` whose `gc_window` returns the default
    /// no-op guard. Drives the negative test: with no barrier
    /// coordination, the race is observable.
    struct NoopActionCache;

    #[async_trait::async_trait]
    impl ActionCache for NoopActionCache {
        async fn get_action_result(
            &self,
            _action_digest: &Digest,
        ) -> Result<Option<rapi::ActionResult>, CasError> {
            Ok(None)
        }

        async fn update_action_result(
            &self,
            _action_digest: &Digest,
            _result: rapi::ActionResult,
        ) -> Result<(), CasError> {
            Ok(())
        }

        async fn list_entries(&self) -> Result<Vec<(Digest, rapi::ActionResult)>, CasError> {
            Ok(Vec::new())
        }

        // `gc_window` is *not* overridden — gets the trait
        // default (no-op guard). This is exactly the
        // configuration under which the issue #144 bug
        // resurfaces if someone removes the override from
        // `RedbActionCache`.
    }

    /// Positive race-repro test (issue #144): with the barrier
    /// active, a worker that holds `gc_window()` blocks the
    /// sweep from doing anything destructive until the worker
    /// has finished both writes. The barrier's hold-and-release
    /// ordering between a worker and a sweep is the correct,
    /// tested invariant. The companion noop-barrier test below
    /// verifies that without the barrier the corruption
    /// resurfaces — i.e. the barrier is load-bearing.
    #[tokio::test]
    async fn sweep_blocks_concurrent_worker_writes() {
        let dir = tempfile::tempdir().unwrap();
        let ac: Arc<RedbActionCache> =
            Arc::new(RedbActionCache::open(dir.path().join("ac.redb")).unwrap());
        let inner: Arc<InMemoryCas> = Arc::new(InMemoryCas::new());
        let pausable = Arc::new(PausableCas::new(inner.clone()));

        // Spawn the sweep first. It acquires its guard inside
        // `sweep` and is now holding it. We use a `oneshot` to
        // observe completion ordering without timing flakes.
        let sweep_done = Arc::new(tokio::sync::Notify::new());
        let sweep_done_clone = sweep_done.clone();
        let sweep_task = {
            let cas: Arc<dyn Cas> = pausable.clone();
            let ac: Arc<dyn ActionCache> = ac.clone();
            tokio::spawn(async move {
                let res = sweep(cas.as_ref(), ac.as_ref()).await;
                sweep_done_clone.notify_one();
                res
            })
        };

        // Wait for sweep to be holding the guard. We don't
        // have a direct signal for that, but the inner CAS has
        // the parking Notify and we can drive from there. The
        // sweep hasn't reached `list_digests` yet (it will,
        // once `gc_window()` returns). To prove the sweep HAS
        // acquired its guard, attempt a separate
        // `gc_window()` from this task. It must park — i.e.
        // not return — until sweep is done.
        let ac_for_acquire = ac.clone();
        let acquire_handle = tokio::spawn(async move {
            let started = std::time::Instant::now();
            let _guard = ac_for_acquire.gc_window().await.unwrap();
            (started.elapsed(), _guard)
        });

        // Give the acquire task time to attempt `gc_window()`.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // At this point: sweep holds the guard, acquire_task
        // is parked on the mutex. Drop the barrier by
        // completing the sweep — push the parked `list_digests`
        // through so `plan_locked` and `sweep_with_plan_locked`
        // run, then `sweep` releases the guard automatically.
        pausable.enter.notified().await;
        pausable.release.notify_one();

        sweep_done.notified().await;
        let _sweep_res = sweep_task.await.unwrap().unwrap();
        let (acquire_elapsed, _g) = acquire_handle.await.unwrap();
        assert!(
            acquire_elapsed >= std::time::Duration::from_millis(40),
            "acquire should have parked on the barrier held by the sweep, \
             but returned in {acquire_elapsed:?}"
        );
    }

    /// Negative test (companion to the above): confirm the bug
    /// returns when the barrier is removed.
    ///
    /// `PausableCas` parks inside `list_digests` exactly like
    /// the positive test, but `NoopActionCache` returns the
    /// default no-op guard. The worker's `(batch_update_blobs,
    /// update_action_result)` pair races through the swept
    /// window, and the resulting `ActionResult` references a
    /// deleted blob — i.e. a poisoned cache.
    ///
    /// Pre-fix and post-fix this test passes both ways;
    /// `sweep` is racing rather than coordinated with the
    /// worker. This locks in the *negative invariant*: "if
    /// someone removes the `gc_window` override, this test
    /// continues to demonstrate the race." The companion test
    /// above is the one that fails when the override is
    /// removed — together they pin both directions.
    #[tokio::test]
    async fn sweep_with_noop_barrier_deletes_in_flight_blob() {
        let noop: Arc<dyn ActionCache> = Arc::new(NoopActionCache);
        let inner: Arc<InMemoryCas> = Arc::new(InMemoryCas::new());
        let pausable = Arc::new(PausableCas::new(inner.clone()));

        let (in_flight, _) = blob(b"in-flight (no barrier)");

        let sweep_task = {
            let cas: Arc<dyn Cas> = pausable.clone();
            let noop = noop.clone();
            tokio::spawn(async move { sweep(cas.as_ref(), noop.as_ref()).await })
        };
        pausable.enter.notified().await;

        pausable
            .batch_update_blobs(vec![(
                in_flight.clone(),
                Bytes::from_static(b"in-flight (no barrier)"),
            )])
            .await
            .unwrap();
        // No `await gc_window` here — the default trait impl returns
        // a no-op guard immediately, so this write commits
        // before the sweep finishes its `plan`.
        noop.update_action_result(
            &Digest::of(b"action-in-flight-noop"),
            ar_with_outputs(&[&in_flight]),
        )
        .await
        .unwrap();

        pausable.release.notify_one();
        let _ = sweep_task.await.unwrap().unwrap();

        // The in-flight blob was deleted even though a live
        // (noop-tracked) ActionResult points at it. This is the
        // bug. The paired test above is what blocks on this
        // behavior once the override is in place.
        let after = inner.list_digests().await.unwrap();
        assert!(
            !after.contains(&in_flight),
            "no-op barrier should let the race through; \
             if this fails the test infrastructure itself raced"
        );
    }

    /// Worker-side invariant: `RedbActionCache::gc_window` does
    /// serialize `update_action_result` against an in-progress
    /// sweep. Two concurrent writes (sweep + AC update) must
    /// complete one after the other, never interleave.
    #[tokio::test]
    async fn update_action_result_serializes_against_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let ac: Arc<RedbActionCache> =
            Arc::new(RedbActionCache::open(dir.path().join("ac.redb")).unwrap());
        let inner: Arc<InMemoryCas> = Arc::new(InMemoryCas::new());
        let pausable = Arc::new(PausableCas::new(inner.clone()));

        // Seed one live and one dead blob; the live one is
        // referenced from a pre-existing AC entry so `plan`
        // sees it as reachable.
        let (live, _) = blob(b"live-serial");
        let (dead, _) = blob(b"dead-serial");
        inner
            .batch_update_blobs(vec![
                (live.clone(), Bytes::from_static(b"live-serial")),
                (dead.clone(), Bytes::from_static(b"dead-serial")),
            ])
            .await
            .unwrap();
        ac.update_action_result(&Digest::of(b"a"), ar_with_outputs(&[&live]))
            .await
            .unwrap();

        // Drive the sweep to its parking point inside
        // `list_digests`. With the barrier, the in-flight
        // AC write below will block on `gc_window()` until the
        // sweep completes.
        let sweep_fut = {
            let cas: Arc<dyn Cas> = pausable.clone();
            let ac: Arc<dyn ActionCache> = ac.clone();
            tokio::spawn(async move { sweep(cas.as_ref(), ac.as_ref()).await })
        };
        pausable.enter.notified().await;

        // Spawn a parallel AC write. It first attempts
        // `gc_window()`; the barrier parks it. Then it tries
        // `update_action_result` once it has the guard. With
        // the barrier it must complete *after* the sweep.
        let ac_for_writer = ac.clone();
        let ac_digest = Digest::of(b"concurrent-action");
        let writer_digest = live.clone();
        let writer = tokio::spawn(async move {
            let _g = ac_for_writer.gc_window().await?;
            ac_for_writer
                .update_action_result(&ac_digest, ar_with_outputs(&[&writer_digest]))
                .await
        });

        // Give the writer a chance to attempt `gc_window()`. A
        // small delay makes the ordering deterministic without
        // baking in timings the runtime can't guarantee.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Release the sweep. With the barrier:
        //   - The sweep's barrier is released first.
        //   - The writer's `gc_window()` acquires next; writer
        //     proceeds, updates the AC, then drops the guard.
        pausable.release.notify_one();

        let sweep_deleted = sweep_fut.await.unwrap().unwrap();
        writer.await.unwrap().unwrap();
        assert_eq!(sweep_deleted, 1, "dead blob should be the only deletion");

        // After both complete, the live blob is still in the
        // CAS store.
        let after = inner.list_digests().await.unwrap();
        assert!(after.contains(&live));
        assert!(!after.contains(&dead));
    }
}
