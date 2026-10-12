//! Metadata-only activity checks for approval-backed cache cleanup.

use std::path::{Component, Path};
use std::time::SystemTime;

use disk_saver_core::{FileKind, Platform};

/// Reject relative paths, traversal, and symlinks in every ancestor.
pub fn real_path(p: &dyn Platform, path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
        && path.ancestors().all(
            |a| matches!(p.metadata(a), Ok(m) if matches!(m.kind, FileKind::File | FileKind::Dir)),
        )
}

/// Latest file modification/access time in a tree, without reading file contents.
/// Directory access times are ignored: the scan itself accesses directories.
/// An unreadable tree, nested mount, or oversized walk fails closed.
pub fn activity(p: &dyn Platform, path: &Path) -> Option<SystemTime> {
    let root_metadata = p.metadata(path).ok()?;
    let mut latest_activity = root_metadata.modified;
    let mut pending_paths = vec![path.to_path_buf()];
    let mut paths_visited = 0;

    while let Some(current_path) = pending_paths.pop() {
        paths_visited += 1;
        if paths_visited > 200_000 {
            return None;
        }

        let metadata = p.metadata(&current_path).ok()?;
        if metadata.dev != root_metadata.dev {
            return None;
        }

        match metadata.kind {
            FileKind::File => {
                latest_activity = latest_activity
                    .max(metadata.modified)
                    .max(metadata.accessed.unwrap_or(metadata.modified))
            }
            FileKind::Dir => {
                latest_activity = latest_activity.max(metadata.modified);
                pending_paths.extend(
                    p.read_dir(&current_path)
                        .ok()?
                        .into_iter()
                        .map(|entry| entry.path),
                );
            }
            FileKind::Symlink => {}
            FileKind::Other => return None,
        }
    }
    Some(latest_activity)
}

/// Render the evidence presented in approval labels.
pub fn access_label(time: SystemTime) -> String {
    format!(
        "last recorded access/activity {} (filesystem proxy, not execution history)",
        humantime::format_rfc3339_seconds(time)
    )
}

/// Record planning evidence using the existing decision-log format.
pub fn record_plan(
    ctx: &disk_saver_core::Ctx,
    name: &str,
    candidates: &[disk_saver_core::Candidate],
) {
    use disk_saver_core::{Decision, DecisionKind, DecisionLog};
    if DecisionLog::compiled() {
        for c in candidates {
            ctx.decide(
                Decision::new(DecisionKind::Planned, name, &c.id)
                    .label(&c.label)
                    .bytes(c.bytes)
                    .age(ctx.now().duration_since(c.last_used).unwrap_or_default()),
            );
        }
    }
}

/// Record execution results; native maintenance reports measured reductions.
pub fn record_results(
    ctx: &disk_saver_core::Ctx,
    name: &str,
    outcomes: &[disk_saver_core::Outcome],
) {
    use disk_saver_core::{Decision, DecisionKind, DecisionLog, Outcome};
    if DecisionLog::compiled() {
        for o in outcomes {
            let decision = match o {
                Outcome::Removed { id, bytes } => {
                    Decision::new(DecisionKind::Deleted, name, id).bytes(*bytes)
                }
                Outcome::Skipped { id, reason } => {
                    Decision::new(DecisionKind::Skipped, name, id).reason(reason)
                }
                Outcome::Failed { id, error } => {
                    Decision::new(DecisionKind::Failed, name, id).reason(error)
                }
            };
            ctx.decide(decision);
        }
    }
}
