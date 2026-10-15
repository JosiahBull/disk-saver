//! [`Claims`] — the paths adapters recognise as their own during a run (§12.8).
//!
//! Two adapters can see the same directory. A `node_modules/` is a `node-modules`
//! artifact and, in a repository that ignores it, a `git-ignored` object too. The
//! specialised adapter judges it by better evidence (project activity, a marker
//! file), so it claims everything it recognises, whether it proposes it or keeps
//! it, and an overlapping adapter that plans later leaves claimed paths alone.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The paths claimed so far in a run, each mapped to the adapter that claimed it.
#[derive(Debug, Default)]
pub struct Claims {
    by_path: BTreeMap<PathBuf, &'static str>,
}

impl Claims {
    /// An empty set.
    pub const fn new() -> Self {
        Claims {
            by_path: BTreeMap::new(),
        }
    }

    /// Record that `adapter` claims `path`, and with it everything beneath it.
    /// The first claim on a path wins.
    pub fn insert(&mut self, path: PathBuf, adapter: &'static str) {
        self.by_path.entry(path).or_insert(adapter);
    }

    /// The adapter that claimed `path` or one of its ancestors, if any.
    pub fn owner(&self, path: &Path) -> Option<&'static str> {
        path.ancestors().find_map(|p| self.by_path.get(p).copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_covers_the_claimed_path_and_its_descendants_only() {
        let mut claims = Claims::new();
        claims.insert(PathBuf::from("/dev/app/node_modules"), "node-modules");

        let owner = |p: &str| claims.owner(Path::new(p));
        assert_eq!(owner("/dev/app/node_modules"), Some("node-modules"));
        assert_eq!(owner("/dev/app/node_modules/.bin"), Some("node-modules"));
        assert_eq!(owner("/dev/app"), None, "an ancestor is not claimed");
        assert_eq!(owner("/dev/app/node_modules_old"), None);
    }

    #[test]
    fn first_claim_wins() {
        let mut claims = Claims::new();
        claims.insert(PathBuf::from("/dev/app/target"), "rust-target");
        claims.insert(PathBuf::from("/dev/app/target"), "other");
        assert_eq!(
            claims.owner(Path::new("/dev/app/target")),
            Some("rust-target")
        );
    }
}
