//! The durable Raft **hard state** — the fields that must survive a crash.

use crate::types::{NodeId, Term};

/// The persistent Raft hard state: the two fields (besides the log) that Raft
/// requires be written to stable storage **before** responding to any RPC that
/// changed them (`docs/raft-notes.md` §3).
///
/// `current_term` and `voted_for` must be updated **atomically as a unit**.
/// When a node advances its term it clears its vote in the *same* step, so a
/// crash can never leave a new term paired with a stale vote — a "torn vote"
/// that would let the node cast two votes in one term and break Election Safety.
/// [`RaftLog::save_hard_state`](crate::RaftLog::save_hard_state) persists both
/// fields in a single durable transaction to guarantee this.
///
/// `commitIndex` and `lastApplied` are deliberately **not** here: they are
/// volatile and safely recomputed after a restart (`docs/raft-notes.md` §3).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HardState {
    /// Latest term the server has seen (initialized to 0, increases
    /// monotonically).
    pub current_term: Term,
    /// The candidate this server voted for in `current_term`, or `None` if it
    /// has not voted this term.
    pub voted_for: Option<NodeId>,
}

impl HardState {
    /// The initial hard state of a fresh node: term 0, no vote.
    pub fn new() -> Self {
        HardState::default()
    }

    /// Returns the hard state advanced to `term` with the vote **cleared** — the
    /// atomic "step down to a higher term" transition (`docs/raft-notes.md`
    /// §2.2). Clearing the vote in the same value is what makes a torn vote
    /// impossible once this is persisted atomically.
    pub fn stepped_to(&self, term: Term) -> Self {
        HardState {
            current_term: term,
            voted_for: None,
        }
    }

    /// Returns the hard state with `voted_for` set to `candidate`, keeping the
    /// current term (used when granting a vote, `docs/raft-notes.md` §4.2).
    pub fn voting_for(&self, candidate: NodeId) -> Self {
        HardState {
            current_term: self.current_term,
            voted_for: Some(candidate),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn default_is_term_zero_no_vote() {
        let hs = HardState::new();
        assert_eq!(hs.current_term, Term::ZERO);
        assert_eq!(hs.voted_for, None);
    }

    #[test]
    fn stepping_to_higher_term_clears_vote() {
        let voted = HardState {
            current_term: Term::new(4),
            voted_for: Some(NodeId::new("cand").unwrap()),
        };
        let stepped = voted.stepped_to(Term::new(5));
        assert_eq!(stepped.current_term, Term::new(5));
        assert_eq!(stepped.voted_for, None, "a term step must clear the vote");
    }

    #[test]
    fn voting_for_keeps_term() {
        let hs = HardState::new().stepped_to(Term::new(3));
        let voted = hs.voting_for(NodeId::new("cand").unwrap());
        assert_eq!(voted.current_term, Term::new(3));
        assert_eq!(voted.voted_for, Some(NodeId::new("cand").unwrap()));
    }
}
