//! Backward-compatibility re-export of [`crate::checks`].
//! Named `host_check` to avoid a naming conflict with the [`run`](crate::checks::run) module
//! in the `checks` module.

pub use crate::checks::run::run as check_run;
pub use crate::checks::{Report as CheckReport, Status};
