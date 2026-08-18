//! The format list the settings tree and the open dialog both read.

use crate::image::decode;

pub fn sorted_format_groups() -> Vec<(&'static str, &'static [&'static str])> {
    let mut groups: Vec<_> = decode::format_groups()
        .chain(crate::archive::reader::format_groups())
        .collect();
    groups.sort_by_key(|(name, _)| *name);
    groups
}
