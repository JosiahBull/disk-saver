//! `disk-saver-adapter-claude-worktrees` — reports, and never removes, the git
//! worktrees Claude Code keeps under `<repo>/.claude/worktrees/`.
//!
//! `claude --worktree`, the `EnterWorktree` tool, and subagents run with
//! `isolation: "worktree"` each check one out there. Claude Code removes its
//! own once they are clean, and keeps any with changes or unpushed commits —
//! along with whatever `target/` or `node_modules/` was built inside. The other
//! filesystem adapters never see those, because the walk skips dot-directories.
//!
//! This adapter finds them under `roots` and reports each one's size in
//! `disk-saver plan`. It deletes nothing: what to keep from an agent's
//! worktree is the agent's call, or yours.

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::path::PathBuf;

use disk_saver_core::{
    Adapter, AdapterError, AdapterFactory, Candidate, ConfigCx, ConfigError, Ctx, FileKind,
    Footprint, Outcome,
};
use disk_saver_scan::worktree::{self, REPORT_ONLY};
use disk_saver_scan::{FsConfig, Rule, find_markers};
use serde::Deserialize;

/// The adapter's stable name (config section, KV bucket, log target).
const NAME: &str = "claude-worktrees";

/// A repository with a `.claude/worktrees` directory.
static RULE: &[Rule] = &[Rule {
    artifact_dir: ".claude",
    required_sibling: &[".git"],
    validate: |p, dir| matches!(p.metadata(&dir.join("worktrees")), Ok(m) if m.kind == FileKind::Dir),
}];

/// `[adapters.claude-worktrees]`: where to look for repositories. There are no
/// age or `confirm` keys, because nothing is ever deleted.
#[derive(Debug, Deserialize)]
#[serde(default)]
struct ClaudeConfig {
    /// Directories to search for repositories. Defaults to `["~"]`.
    roots: Vec<PathBuf>,
    /// Absolute-path globs to skip, matching a repository or a single worktree.
    exclude: Vec<String>,
    /// How far below each root to look for repositories. Defaults to `8`.
    max_depth: usize,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        let fs = FsConfig::default();
        ClaudeConfig {
            roots: fs.roots,
            exclude: fs.exclude,
            max_depth: fs.max_depth,
        }
    }
}

impl ClaudeConfig {
    /// Project onto the shared [`FsConfig`] for root/exclude resolution.
    fn as_fs(&self) -> FsConfig {
        FsConfig {
            roots: self.roots.clone(),
            exclude: self.exclude.clone(),
            max_depth: self.max_depth,
            ..FsConfig::default()
        }
    }
}

/// The factory the CLI registry uses to build the claude-worktrees adapter.
pub fn factory() -> AdapterFactory {
    AdapterFactory::typed(NAME, build)
}

fn build(cfg: ClaudeConfig, cx: &ConfigCx) -> Result<ClaudeWorktrees, ConfigError> {
    let fs = cfg.as_fs();
    cx.wrap(fs.glob_set(), "invalid exclude glob")?;
    Ok(ClaudeWorktrees { fs })
}

struct ClaudeWorktrees {
    fs: FsConfig,
}

impl Adapter for ClaudeWorktrees {
    fn name(&self) -> &'static str {
        NAME
    }

    fn observe(&mut self, _ctx: &mut Ctx) -> Result<(), AdapterError> {
        Ok(())
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
        let (roots, opts) = self
            .fs
            .resolve_walk(ctx.platform)
            .map_err(|e| AdapterError::Failed(anyhow::anyhow!("compiling exclude globs: {e}")))?;

        // Overlapping roots find the same repository twice; count it once.
        let mut seen = HashSet::new();
        let mut footprints = Vec::new();
        for marker in find_markers(ctx.platform, &roots, RULE, &opts) {
            let Ok(entries) = ctx.platform.read_dir(&marker.dir.join("worktrees")) else {
                continue;
            };
            for entry in entries {
                if entry.kind != FileKind::Dir
                    || opts.exclude.is_match(&entry.path)
                    || !seen.insert(entry.path.clone())
                {
                    continue;
                }
                let wt = worktree::inspect(ctx.platform, &entry.path);
                footprints.push(Footprint::new(
                    entry.path.to_string_lossy(),
                    wt.label(),
                    wt.size,
                    wt.last_active,
                ));
            }
        }
        Ok(footprints)
    }
}
