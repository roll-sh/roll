//! Merging: which segments to merge ([`policy`]) and how merged docs are ordered ([`sort`]).
//! Merge execution lives with the shard (it installs results under the writer lock).

pub mod policy;
pub mod sort;
