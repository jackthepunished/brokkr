//! Pass / warn / fail outcome of a single probe.

/// Pass / warn / fail outcome of a single probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Passes; no concern.
    Pass,
    /// Functional but degraded — a fallback path will be used.
    Warn,
    /// Sandbox cannot start without this fixed.
    Fail,
}

impl Status {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Status::Pass => " OK  ",
            Status::Warn => "WARN ",
            Status::Fail => "FAIL ",
        }
    }
}
