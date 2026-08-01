//! `LocalityAware` — an example Brokkr scheduling policy (ADR 0014).
//!
//! ADR 0008 promised a built-in `LocalityAware` and never built it. This is
//! that policy, delivered as an *operator-editable module* instead: the
//! tradeoff it encodes — how much a warm cache is worth relative to an idle
//! worker — is a property of your fleet, not of Brokkr, and you should be able
//! to change it without a rebuild.
//!
//! # The policy
//!
//! Score each candidate and take the highest, breaking ties by the earliest
//! position (the host sorts candidates by worker id, so ties resolve
//! deterministically):
//!
//! ```text
//! score = INPUT_ROOT_WEIGHT * input_root_hits
//!       + ACTION_WEIGHT     * action_hits
//!       - LOAD_WEIGHT       * inflight
//! ```
//!
//! A worker that recently ran an action with this same input root very likely
//! still has those inputs materialized, so sending it the next such action
//! skips a fetch. `action_hits` is a weaker signal pointing the same way. Load
//! pulls the other direction, so a warm-but-saturated worker eventually loses
//! to a cold idle one — which is the whole point of having weights rather than
//! a rule.
//!
//! **Tune the three constants below and rebuild.** That is the intended
//! workflow; the control plane picks the new module up within
//! `--policy-reload-interval-secs`.
//!
//! # Building
//!
//! ```sh
//! rustup target add wasm32-unknown-unknown
//! cargo build --release --target wasm32-unknown-unknown
//! # then point the control plane at it:
//! brokkr-control --policy-wasm target/wasm32-unknown-unknown/release/brokkr_policy_locality.wasm
//! ```
//!
//! The `.wasm` is not committed to the repository (CLAUDE.md rule 4); build it.

// `std` rather than `no_std`: this is an example an operator is meant to read
// and edit, and `no_std` would buy a slightly smaller module in exchange for a
// hand-rolled global allocator. For a scheduling policy the size is irrelevant
// and the clarity is not.

use prost::Message;

/// ABI version this policy is compiled against. The host refuses a module that
/// disagrees, rather than letting it misparse snapshots silently.
const ABI_VERSION: i32 = 1;

/// Returned to mean "no preference — use the built-in for this decision".
const DECLINE: i32 = -1;

/// How much one recent completion on the same **input root** is worth. The
/// strongest signal: same inputs means the materialized tree is probably still
/// on disk.
const INPUT_ROOT_WEIGHT: i64 = 10;

/// How much one recent completion of the **same action** is worth. Weaker, and
/// largely subsumed by the input-root signal, but it survives an input-root
/// digest the control plane could not parse.
const ACTION_WEIGHT: i64 = 3;

/// How much one in-flight job counts against a worker. Raise this to spread
/// more aggressively; lower it to chase cache warmth harder.
const LOAD_WEIGHT: i64 = 4;

// The subset of `brokkr/v1/policy.proto` this policy reads. Declared by hand
// rather than generated, so the example has no build script and no protoc
// dependency — and so the field numbers you must match are visible in one
// place. Unknown fields are skipped by prost, so the host adding a field
// later does not break this module.

/// Mirrors `brokkr.v1.PolicyJobFacts`.
#[derive(Clone, PartialEq, Message)]
struct JobFacts {
    #[prost(string, tag = "1")]
    tenant: String,
    #[prost(string, tag = "2")]
    action_digest: String,
    #[prost(string, tag = "3")]
    input_root_digest: String,
    // Field 4 (platform) is not read by this policy; prost skips it.
}

/// Mirrors `brokkr.v1.PolicyCandidate`.
#[derive(Clone, PartialEq, Message)]
struct Candidate {
    #[prost(string, tag = "1")]
    worker_id: String,
    #[prost(uint32, tag = "2")]
    inflight: u32,
    // Field 3 (labels) is not read by this policy.
    #[prost(uint32, tag = "4")]
    input_root_hits: u32,
    #[prost(uint32, tag = "5")]
    action_hits: u32,
}

/// Mirrors `brokkr.v1.DecisionSnapshot`.
#[derive(Clone, PartialEq, Message)]
struct DecisionSnapshot {
    #[prost(uint32, tag = "1")]
    abi_version: u32,
    #[prost(message, optional, tag = "2")]
    job: Option<JobFacts>,
    #[prost(message, repeated, tag = "3")]
    candidates: Vec<Candidate>,
}

/// Report the ABI version this module speaks.
///
/// # Safety
///
/// Called by the host with no arguments; nothing unsafe happens here. The
/// `extern "C"` is only to give the export a stable calling convention.
#[no_mangle]
pub extern "C" fn brokkr_abi_version() -> i32 {
    ABI_VERSION
}

/// Allocate `len` bytes for the host to write the snapshot into.
///
/// The host never frees: it drops the whole store after the call, which
/// reclaims the entire linear memory at once. So this deliberately leaks, and
/// that is correct rather than sloppy — a free would be pure overhead on a
/// heap that is about to be discarded wholesale.
#[no_mangle]
pub extern "C" fn brokkr_alloc(len: i32) -> i32 {
    if len < 0 {
        return 0;
    }
    let mut buf: Vec<u8> = Vec::with_capacity(len as usize);
    let ptr = buf.as_mut_ptr();
    core::mem::forget(buf);
    ptr as i32
}

/// Choose a candidate for this job.
///
/// Returns the candidate's index, or [`DECLINE`] to let the control plane use
/// its built-in strategy.
#[no_mangle]
pub extern "C" fn brokkr_choose(ptr: i32, len: i32) -> i32 {
    if ptr <= 0 || len <= 0 {
        return DECLINE;
    }
    // SAFETY: the host wrote exactly `len` bytes at `ptr`, which is the
    // pointer `brokkr_alloc` returned for a buffer of that length, and the
    // memory lives until the store is dropped after this call returns. The
    // slice is read-only and does not outlive this function.
    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };

    let Ok(snapshot) = DecisionSnapshot::decode(bytes) else {
        // A snapshot we cannot parse is not something to guess about.
        return DECLINE;
    };
    if snapshot.abi_version != ABI_VERSION as u32 || snapshot.candidates.is_empty() {
        return DECLINE;
    }
    // With no input root there is no locality signal worth acting on, and the
    // built-in already handles pure load better than this scoring would.
    let has_input_root = snapshot
        .job
        .as_ref()
        .is_some_and(|j| !j.input_root_digest.is_empty());
    if !has_input_root {
        return DECLINE;
    }

    let mut best_index = 0usize;
    let mut best_score = i64::MIN;
    for (i, c) in snapshot.candidates.iter().enumerate() {
        let score = INPUT_ROOT_WEIGHT * i64::from(c.input_root_hits)
            + ACTION_WEIGHT * i64::from(c.action_hits)
            - LOAD_WEIGHT * i64::from(c.inflight);
        // Strictly greater, so ties keep the earliest candidate. The host sorts
        // candidates by worker id, so that is a stable, explainable tie-break.
        if score > best_score {
            best_score = score;
            best_index = i;
        }
    }

    // `candidates.len()` came from a decoded message and could in principle
    // exceed i32; clamp rather than wrap, since a wrapped index would be a
    // wrong placement rather than an error.
    i32::try_from(best_index).unwrap_or(DECLINE)
}

// `panic = "abort"` in the release profile turns any panic in here into a
// WebAssembly trap, which the host classifies, counts, and falls back from — so
// a bug in this policy degrades placement rather than taking anything down.
