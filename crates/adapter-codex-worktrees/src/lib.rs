//! `disk-saver-adapter-codex-worktrees` — reports, and never removes, the git
//! worktrees OpenAI Codex keeps under `$CODEX_HOME/worktrees/`.
//!
//! Codex checks each worktree out at `<root>/<id>/<repo>`, where `<id>` is
//! four hex digits or a name. The app removes old ones itself: it keeps the
//! newest `worktree-keep-count`, snapshots each one's changes before removing
//! it, and leaves the `<id>` directory behind so it can restore the checkout to
//! the same path. Removing a checkout out from under it would skip that
//! snapshot, so this adapter only measures — each checkout's size appears in
//! `disk-saver plan` — and the empty `<id>` directories are passed over.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use disk_saver_core::{
    Adapter, AdapterError, AdapterFactory, Candidate, ConfigCx, ConfigError, Ctx, FileKind,
    Footprint, Outcome, Platform, expand_tilde,
};
use disk_saver_scan::worktree::{self, REPORT_ONLY};
use serde::Deserialize;

/// The adapter's stable name (config section, KV bucket, log target).
const NAME: &str = "codex-worktrees";

/// `[adapters.codex-worktrees]`. There are no age or `confirm` keys, because
/// nothing is ever deleted.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CodexConfig {
    /// Codex's worktree root. Defaults to `$CODEX_HOME/worktrees`, else
    /// `~/.codex/worktrees`; set it if the root was moved in Codex's settings.
    root: Option<PathBuf>,
}

/// The factory the CLI registry uses to build the codex-worktrees adapter.
pub fn factory() -> AdapterFactory {
    AdapterFactory::typed(NAME, build)
}

fn build(cfg: CodexConfig, _cx: &ConfigCx) -> Result<CodexWorktrees, ConfigError> {
    Ok(CodexWorktrees { cfg })
}

struct CodexWorktrees {
    cfg: CodexConfig,
}

impl CodexWorktrees {
    /// The configured root, else `$CODEX_HOME/worktrees`, else
    /// `~/.codex/worktrees`.
    fn root(&self, p: &dyn Platform) -> PathBuf {
        if let Some(root) = &self.cfg.root {
            return expand_tilde(root, &p.home_dir());
        }
        match p.env_var("CODEX_HOME").filter(|home| !home.is_empty()) {
            Some(home) => PathBuf::from(home).join("worktrees"),
            None => p.home_dir().join(".codex").join("worktrees"),
        }
    }
}

/// `Unavailable` unless `root` is a directory: no root means no Codex here.
fn ensure_root(p: &dyn Platform, root: &Path) -> Result<(), AdapterError> {
    match p.metadata(root) {
        Ok(m) if m.kind == FileKind::Dir => Ok(()),
        _ => Err(AdapterError::unavailable(format!(
            "no Codex worktrees at {}",
            root.display()
        ))),
    }
}

impl Adapter for CodexWorktrees {
    fn name(&self) -> &'static str {
        NAME
    }

    fn check(&mut self, ctx: &mut Ctx) -> Result<(), AdapterError> {
        ensure_root(ctx.platform, &self.root(ctx.platform))
    }

    fn observe(&mut self, ctx: &mut Ctx) -> Result<(), AdapterError> {
        ensure_root(ctx.platform, &self.root(ctx.platform))
    }

    /// Report-only: nothing is ever a candidate.
    fn plan(&mut self, _ctx: &mut Ctx) -> Result<Vec<Candidate>, AdapterError> {
        Ok(Vec::new())
    }

    fn execute(
        &mut self,
        _ctx: &mut Ctx,
        batch: &[Candidate],
    ) -> Result<Vec<Outcome>, AdapterError> {
        Ok(batch
            .iter()
            .map(|c| Outcome::Skipped {
                id: c.id.clone(),
                reason: REPORT_ONLY.to_owned(),
            })
            .collect())
    }

    fn report(&mut self, ctx: &mut Ctx) -> Result<Vec<Footprint>, AdapterError> {
        let p = ctx.platform;
        let root = self.root(p);
        let buckets = p.read_dir(&root).map_err(|e| {
            AdapterError::unavailable(format!("cannot read {}: {e}", root.display()))
        })?;

        let mut footprints = Vec::new();
        for bucket in buckets.iter().filter(|e| e.kind == FileKind::Dir) {
            // A bucket Codex has already emptied holds only its name marker.
            let Ok(checkouts) = p.read_dir(&bucket.path) else {
                continue;
            };
            for checkout in checkouts.iter().filter(|e| e.kind == FileKind::Dir) {
                let wt = worktree::inspect(p, &checkout.path);
                footprints.push(Footprint::new(
                    checkout.path.to_string_lossy(),
                    wt.label(),
                    wt.size,
                    wt.last_active,
                ));
            }
        }
        Ok(footprints)
    }
}
