//! Individually age completed download archives, preserving in-progress files.

use std::path::{Path, PathBuf};

use disk_saver_core::{
    Adapter, AdapterError, AdapterFactory, Candidate, Class, ConfigCx, Ctx, FileKind, Outcome,
    Platform, RetentionPolicy,
};

use crate::native::NativeConfig;
use crate::safety::{access_label, real_path, record_plan, record_results};

/// Resolve the tool's download directory.
pub type DownloadRoot = fn(&dyn Platform, Option<&Path>) -> PathBuf;

/// Recognise a completed cache archive by its filename.
pub type ArchiveName = fn(&str) -> bool;

/// Build an adapter that only removes independently aged completed downloads.
pub fn factory(name: &'static str, root: DownloadRoot, archive: ArchiveName) -> AdapterFactory {
    AdapterFactory::typed(name, move |cfg: NativeConfig, cx: &ConfigCx| {
        Ok(Downloads {
            name,
            root,
            archive,
            policy: cx.retention(cfg.max_age, cfg.min_age)?,
            cfg,
        })
    })
}

/// Shared completed-download adapter, also embedded by nvm.
pub struct Downloads {
    /// Adapter name.
    pub name: &'static str,
    /// Download-root resolver.
    pub root: DownloadRoot,
    /// Completed-archive filename predicate.
    pub archive: ArchiveName,
    /// Config.
    pub cfg: NativeConfig,
    /// Validated age policy.
    pub policy: RetentionPolicy,
}

impl Adapter for Downloads {
    fn name(&self) -> &'static str {
        self.name
    }

    fn observe(&mut self, _: &mut Ctx) -> Result<(), AdapterError> {
        Ok(())
    }

    fn plan(&mut self, ctx: &mut Ctx) -> Result<Vec<Candidate>, AdapterError> {
        let root = (self.root)(ctx.platform, self.cfg.cache_dir.as_deref());
        if !real_path(ctx.platform, &root) {
            return Ok(vec![]);
        }

        let root_device = ctx
            .platform
            .metadata(&root)
            .map_err(|e| AdapterError::unavailable(e.to_string()))?
            .dev;

        let mut pending_dirs = vec![(root, 0)];
        let mut candidates = vec![];
        let mut entries_visited = 0;

        while let Some((directory, depth)) = pending_dirs.pop() {
            for entry in ctx
                .platform
                .read_dir(&directory)
                .map_err(|e| AdapterError::unavailable(e.to_string()))?
            {
                entries_visited += 1;
                if entries_visited > 20_000 {
                    return Err(AdapterError::unavailable("download scan limit exceeded"));
                }

                let metadata = ctx
                    .platform
                    .metadata(&entry.path)
                    .map_err(|e| AdapterError::unavailable(e.to_string()))?;

                if metadata.dev != root_device {
                    continue;
                }

                if metadata.kind == FileKind::Dir && depth < 3 && !entry.file_name.starts_with('.')
                {
                    pending_dirs.push((entry.path, depth + 1));
                } else if metadata.kind == FileKind::File && (self.archive)(&entry.file_name) {
                    let Some(accessed) = metadata.accessed else {
                        continue;
                    };

                    let last_activity = accessed.max(metadata.modified);

                    if metadata.len > 0
                        && self.policy.eligible(
                            ctx.now().duration_since(last_activity).unwrap_or_default(),
                            ctx.pressure,
                        )
                    {
                        candidates.push(
                            Candidate::new(
                                entry.path.to_string_lossy(),
                                format!(
                                    "{} download: {}; {}",
                                    self.name,
                                    entry.path.display(),
                                    access_label(last_activity)
                                ),
                                metadata.len,
                                last_activity,
                                Class::Cache,
                            )
                            .confirm(self.cfg.confirm),
                        );
                    }
                }
            }
        }

        record_plan(ctx, self.name, &candidates);
        Ok(candidates)
    }

    fn execute(
        &mut self,
        ctx: &mut Ctx,
        batch: &[Candidate],
    ) -> Result<Vec<Outcome>, AdapterError> {
        let root = (self.root)(ctx.platform, self.cfg.cache_dir.as_deref());
        let root_dev = ctx.platform.metadata(&root).ok().map(|m| m.dev);

        let outcomes: Vec<_> = batch
            .iter()
            .map(|c| {
                let path = Path::new(&c.id);
                let skip = |reason: &str| Outcome::Skipped {
                    id: c.id.clone(),
                    reason: reason.into(),
                };

                if !path.starts_with(&root)
                    || path == root
                    || !real_path(ctx.platform, path)
                    || !path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .is_some_and(self.archive)
                {
                    return skip("not a recognised download in the configured cache");
                }

                let Ok(metadata) = ctx.platform.metadata(path) else {
                    return skip("download disappeared");
                };

                let Some(accessed) = metadata.accessed else {
                    return skip("access timestamp unavailable");
                };

                if metadata.kind != FileKind::File
                    || Some(metadata.dev) != root_dev
                    || metadata.len != c.bytes
                    || accessed.max(metadata.modified) > c.last_used
                {
                    return skip("download changed or was accessed since planning");
                }

                // Avoid removing an archive currently being read by an installer.
                match ctx.platform.run_command(&disk_saver_core::CommandSpec::new(
                    "lsof",
                    ["-t", "--", &c.id],
                )) {
                    Ok(o) if o.status == 1 && o.stdout.is_empty() && o.stderr.is_empty() => {}
                    _ => return skip("download may be open, or open-file check unavailable"),
                }

                match ctx.platform.remove_file(path) {
                    Ok(()) => Outcome::Removed {
                        id: c.id.clone(),
                        bytes: metadata.len,
                    },
                    Err(e) => Outcome::Failed {
                        id: c.id.clone(),
                        error: e.to_string(),
                    },
                }
            })
            .collect();

        record_results(ctx, self.name, &outcomes);
        Ok(outcomes)
    }
}

/// Recognise archives without admitting lock files or partial downloads.
pub fn archive_name(name: &str) -> bool {
    !name.contains(".incomplete")
        && !name.contains(".partial")
        && !name.contains(".lock")
        && [
            ".tar.bz2", ".tar.gz", ".tar.xz", ".tar.zst", ".zip", ".tgz", ".dmg", ".pkg", ".gz",
            ".xz",
        ]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}
