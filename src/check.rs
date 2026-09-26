use std::collections::HashMap;
use std::path::PathBuf;

use super::metadata::Meta;

/// A loaded check. Id = file name inside the watched dir (symlinks allowed).
#[derive(Debug, Clone)]
pub struct Check {
    #[allow(dead_code)]
    pub id: String,
    pub path: PathBuf,
    pub meta: Meta,
    /// bumped on every (re)load; invalidates stale scheduler heap entries
    pub version: u64,
    /// generation of the in-flight run, if any
    pub running_gen: Option<u64>,
}

impl Check {
    pub fn new(id: String, path: PathBuf, meta: Meta, version: u64) -> Self {
        Check {
            id,
            path,
            meta,
            version,
            running_gen: None,
        }
    }
}

pub type Checks = HashMap<String, Check>;
