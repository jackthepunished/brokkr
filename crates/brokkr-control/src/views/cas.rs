//! CAS projections.

use brokkr_cas::CasStats;

/// One node's CAS size, as an operator sees it.
///
/// **Never summed across nodes.** Each control-plane node opens its own CAS
/// (`RedbCas::open(data_dir/cas.redb)`), so the same blob present on three
/// nodes is three copies of one blob. Adding the numbers together would report
/// storage that does not exist and a dedup ratio that means nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CasStatsView {
    /// Distinct blobs in this node's store.
    pub objects: u64,
    /// Bytes in this node's store.
    pub bytes: u64,
    /// The control-plane node this store belongs to.
    pub owning_node: String,
}

/// Project one node's [`CasStats`] into a [`CasStatsView`].
pub fn cas_stats_view(stats: CasStats, owning_node: &str) -> CasStatsView {
    CasStatsView {
        objects: stats.objects,
        bytes: stats.bytes,
        owning_node: owning_node.to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn the_view_carries_the_owning_node() {
        let v = cas_stats_view(
            CasStats {
                objects: 3,
                bytes: 900,
            },
            "node-2",
        );
        assert_eq!(v.objects, 3);
        assert_eq!(v.bytes, 900);
        assert_eq!(v.owning_node, "node-2");
    }
}
