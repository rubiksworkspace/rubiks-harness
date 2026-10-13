// Copied from Rubiks IDE (a Zed fork), crates/collections/src/collections.rs, at commit 908f62b01c.
// Only the items the Zed CLI uses are kept; see vendor/zed-cli/README.md.

pub type HashMap<K, V> = FxHashMap<K, V>;
pub type HashSet<T> = FxHashSet<T>;

pub use rustc_hash::{FxBuildHasher, FxHashMap, FxHashSet, FxHasher};
pub use std::collections::*;
