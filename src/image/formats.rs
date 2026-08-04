//! The format list the settings tree and the open dialog both read.

use crate::image::decode;

/// Format groups the association tree and the open dialog both list, in one shared order.
pub fn sorted_format_groups(
    include_archives: bool,
) -> Vec<(&'static str, &'static [&'static str])> {
    let mut groups: Vec<_> = decode::format_groups().collect();
    if include_archives {
        groups.extend(crate::archive::reader::format_groups());
    }
    groups.sort_by_key(|(name, _)| *name);
    groups
}
