# 0006 — License: Apache-2.0

- **Status:** accepted
- **Date:** 2026-04-30
- **Deciders:** Brokkr maintainers

## Context

Brokkr is open-source server infrastructure that executes untrusted
code, will eventually run multi-tenant, and aspires to be embedded by
build tools and CI systems at organizations that scrutinize licenses
during procurement. The license is therefore not a ceremonial choice
— it shapes:

- **Patent risk** for contributors and downstream adopters.
- **Enterprise adoption** (most procurement teams allowlist
  permissive licenses, blocklist copyleft).
- **Trademark protection** for the "Brokkr" name.
- **Compatibility** with the rest of the REAPI ecosystem
  (Bazel itself is Apache-2.0; buildbarn is Apache-2.0;
  bazel-remote is BSD-3).
- **Contributor onboarding** — the inbound license rules whether a CLA
  is needed.

The Rust ecosystem convention is **dual MIT / Apache-2.0**, but that
convention exists primarily so the Rust *toolchain* can be embedded
freely in any downstream regardless of patent posture. Brokkr is a
server, not a toolchain — the convention is informative but not
load-bearing here.

## Decision

License everything in the workspace under **Apache-2.0**, applied
uniformly:

- `LICENSE` at the repo root contains the full Apache-2.0 text.
- `Cargo.toml` `[workspace.package]` sets `license = "Apache-2.0"`.
- Every crate inherits the workspace license; no per-crate license
  overrides.
- Source files do **not** require per-file SPDX headers, but new files
  may include `// SPDX-License-Identifier: Apache-2.0` at author's
  discretion.
- Third-party assets (vendored protos, fixtures) keep their original
  licenses; aggregate `NOTICE` file is added when the first such
  attribution accumulates.

## Alternatives considered

- **MIT alone.**
  - Pros: shortest, most familiar, smallest legal surface.
  - Cons: **no explicit patent grant** — leaves contributors and
    downstream exposed to patent-ambush by past contributors. For a
    project that runs untrusted code and may be embedded in commercial
    CI products, this is the dealbreaker.

- **Dual MIT / Apache-2.0** (Rust ecosystem convention).
  - Pros: matches every published Rust crate; downstream picks
    whichever fits their context; SPDX `MIT OR Apache-2.0` is
    universally understood.
  - Cons: the convention exists for *toolchain portability* — that is
    not Brokkr's situation; doubles the LICENSE files; complicates
    contributor IP attribution because every contribution is
    simultaneously dual-licensed; we get no benefit Apache-2.0 alone
    does not already give us for *server* software.

- **MPL-2.0 (Mozilla Public License).**
  - Pros: file-level copyleft strikes a middle ground; common in
    infrastructure (e.g., HashiCorp until 2023).
  - Cons: less familiar to procurement; weaker patent grant than
    Apache-2.0; recent industry sentiment has moved away from
    file-level copyleft for infra.

- **GPL-3.0.**
  - Pros: forces downstream to contribute back; aligns with
    free-software ideals.
  - Cons: incompatible with most enterprise procurement; deters the
    "point Bazel at it" use case; closes the door on commercial
    embeddings (which we may want, even if we are not building a
    SaaS).

- **AGPL-3.0.**
  - Pros: closes the SaaS loophole; ensures network-deployed forks
    contribute back.
  - Cons: enterprise procurement actively blocklists AGPL; "deployment
    as distribution" semantics frighten self-hosters; severe adoption
    headwind for a project whose value is "self-host this and point
    Bazel at it."

- **BUSL (Business Source License).**
  - Pros: source-available with a delayed open-source conversion;
    protects against direct SaaS competition during the early years.
  - Cons: not OSI-approved (so not "open source" in the strict sense);
    alienates contributors; useful only when there is a SaaS
    competitor to fend off — premature for Brokkr; the project axioms
    do not require SaaS-defense.

- **Elastic License v2 / SSPL.**
  - Cons: same problems as BUSL; community sentiment in 2024–2026 has
    been openly hostile after the Elastic and MongoDB precedents;
    not a fit.

## Consequences

### Positive

- **Explicit patent grant.** Apache-2.0 §3 grants a patent license
  from contributors to downstream users. Without this, any
  contributor's employer could in principle sue downstream adopters
  over patents covering contributed code.
- **Trademark reservation.** Apache-2.0 §6 explicitly does not grant
  trademark rights — preserves our ability to defend the "Brokkr"
  name against forks claiming endorsement.
- **Enterprise-friendly.** Apache-2.0 is on every major
  procurement-team allowlist (CNCF, Apache Software Foundation, AWS,
  Google, Microsoft all default to it). Removes the most common
  adoption friction.
- **REAPI ecosystem fit.** Bazel (Apache-2.0), buildbarn
  (Apache-2.0), and BuildBuddy (MIT) are all permissively licensed;
  we integrate cleanly.
- **Inbound = outbound.** Per Apache-2.0 §5, contributions are
  inbound under the same terms unless explicitly stated otherwise.
  No CLA required during the solo phase.
- **One license, one SPDX expression.** Easier `cargo deny check
  licenses`; simpler audit; no choice paralysis when filing PRs.

### Negative

- **Not GPLv2-compatible.** GPLv2-only code cannot link Apache-2.0
  code (the patent terms conflict). GPLv3 is fine. We never depend
  on GPLv2-only Rust crates; `deny.toml` will enforce this.
- **Per-file headers convention.** Apache-2.0 traditionally expects
  per-file copyright headers. We waive this for code clarity and
  rely on the root `LICENSE` and `Cargo.toml` metadata; this is a
  defensible reading of the license but worth noting.
- **No "viral" protection.** Forks may take Brokkr private. Accepted
  as the cost of broad adoption; the project axioms do not include
  "force downstream contribution."

### Neutral

- **Crate publishing readiness** (Phase 4+). `license = "Apache-2.0"`
  is already set in the workspace; nothing changes when we publish to
  crates.io.
- **Future re-licensing** would require contributor consent from
  every author. Manageable in the solo phase; if external
  contributors arrive, we will add a CLA before accepting their
  patches if re-licensing remains a possibility. **No such CLA is
  required today.**
- **Vendored REAPI protos** (`crates/brokkr-proto/protos/`) carry
  their upstream Apache-2.0 license; combination is straightforward
  and a `NOTICE` entry will be added when we cut a release.

## References

- Apache License 2.0 text: <https://www.apache.org/licenses/LICENSE-2.0>
- "Choose a License" overview: <https://choosealicense.com/licenses/apache-2.0/>
- Rust dual-licensing rationale (historical):
  <https://internals.rust-lang.org/t/rationale-of-dual-licensing-mit-x11-asl2/8794>
- `docs/plan.md` §31 Open Questions (license item, now resolved by
  this ADR).
- Workspace metadata: `Cargo.toml` `[workspace.package]`.
- REAPI license posture:
  <https://github.com/bazelbuild/remote-apis/blob/main/LICENSE>
