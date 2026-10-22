//! Red reference only: the surface the actor health tests call, with no
//! behavior, so they compile and fail on behavior alone.
#![cfg(test)]

use cccc_core::GroupDoc;
use std::collections::BTreeMap;
use std::path::Path;

pub(crate) fn ignored_after_seconds(_group: &GroupDoc) -> i64 {
    1800
}

pub(super) fn forget(_group_id: &str) {}

pub(super) fn replay(
    _path: &Path,
    _now: i64,
    _ignored_after: i64,
    _count_after: i64,
) -> BTreeMap<String, (usize, usize)> {
    BTreeMap::new()
}
