//! Native cache maintenance, with a read-only planning phase.

use std::path::{Path, PathBuf};
use std::time::Duration;

use disk_saver_core::{
    Adapter, AdapterError, AdapterFactory, Candidate, Class, CommandSpec, ConfigCx, Ctx, FileKind,
    Outcome, Platform, RetentionPolicy, expand_tilde,
};
use serde::Deserialize;

use crate::safety::{access_label, activity, real_path, record_plan, record_results};

/// Config for native cache maintenance. New cache adapters ask before cleaning.
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NativeConfig {
    /// Normal-mode inactivity threshold.
    #[serde(with = "humantime_serde")]
    pub max_age: Duration,
    /// Scavenge inactivity floor.
    #[serde(with = "humantime_serde")]
    pub min_age: Duration,
    /// Queue maintenance for approval.
    pub confirm: bool,
    /// Explicit cache root override, otherwise query the tool.
    pub cache_dir: Option<PathBuf>,
}

impl Default for NativeConfig {
    fn default() -> Self {
        Self {
            max_age: Duration::from_secs(30 * 86400),
            min_age: Duration::from_secs(7 * 86400),
            confirm: true,
            cache_dir: None,
        }
    }
}

/// The native commands and the artifact subtree they maintain.
#[derive(Clone, Copy)]
pub enum Tool {
    /// uv's unused-entry pruning.
    Uv,
    /// npm's content-addressed download cache only.
    Npm,
}

impl Tool {
    fn name(self) -> &'static str {
        match self {
            Self::Uv => "uv",
            Self::Npm => "npm",
        }
    }

    fn discover(self) -> CommandSpec {
        match self {
            Self::Uv => CommandSpec::new("uv", ["cache", "dir"]),
            Self::Npm => CommandSpec::new("npm", ["config", "get", "cache"]),
        }
    }

    fn artifact(self, root: &Path) -> PathBuf {
        match self {
            Self::Uv => root.to_path_buf(),
            Self::Npm => root.join("_cacache"),
        }
    }

    fn clean(self, root: &Path) -> CommandSpec {
        let root = root.to_string_lossy().into_owned();
        match self {
            Self::Uv => CommandSpec::new("uv", ["cache", "prune", "--cache-dir", &root]),
            Self::Npm => CommandSpec::new("npm", ["--cache", &root, "cache", "clean", "--force"]),
        }
        .with_timeout(Duration::from_secs(360))
    }
}

/// Build a native cache adapter.
pub fn factory(tool: Tool) -> AdapterFactory {
    AdapterFactory::typed(tool.name(), move |cfg: NativeConfig, cx: &ConfigCx| {
        Ok(NativeAdapter {
            tool,
            policy: cx.retention(cfg.max_age, cfg.min_age)?,
            cfg,
        })
    })
}

struct NativeAdapter {
    tool: Tool,
    cfg: NativeConfig,
    policy: RetentionPolicy,
}

impl NativeAdapter {
    fn root(&self, p: &dyn Platform) -> Result<PathBuf, AdapterError> {
        let cache_root = if let Some(configured_root) = &self.cfg.cache_dir {
            expand_tilde(configured_root, &p.home_dir())
        } else {
            let discovery = p
                .run_command(&self.tool.discover())
                .map_err(|e| AdapterError::unavailable(e.to_string()))?;
            if !discovery.success() {
                return Err(AdapterError::unavailable("cannot locate tool cache"));
            }
            let output = discovery.stdout_string();
            if output.trim().lines().count() != 1 {
                return Err(AdapterError::unavailable("invalid cache path from tool"));
            }
            PathBuf::from(output.trim())
        };
        if !real_path(p, &cache_root) || cache_root == p.home_dir() || cache_root.parent().is_none()
        {
            return Err(AdapterError::unavailable(
                "cache path is unsafe or unreadable",
            ));
        }
        Ok(cache_root)
    }
}

impl Adapter for NativeAdapter {
    fn name(&self) -> &'static str {
        self.tool.name()
    }

    fn observe(&mut self, ctx: &mut Ctx) -> Result<(), AdapterError> {
        self.root(ctx.platform).map(|_| ())
    }

    fn plan(&mut self, ctx: &mut Ctx) -> Result<Vec<Candidate>, AdapterError> {
        if matches!(ctx.pressure, disk_saver_core::Pressure::Comfortable) {
            return Ok(vec![]);
        }
        let root = self.root(ctx.platform)?;
        let artifact = self.tool.artifact(&root);
        if !real_path(ctx.platform, &artifact)
            || !matches!(
                ctx.platform.metadata(&artifact),
                Ok(metadata) if metadata.kind == FileKind::Dir
            )
        {
            return Ok(vec![]);
        }
        let Some(last_activity) = activity(ctx.platform, &artifact) else {
            return Err(AdapterError::unavailable("cannot establish cache activity"));
        };
        if !self.policy.eligible(
            ctx.now().duration_since(last_activity).unwrap_or_default(),
            ctx.pressure,
        ) {
            return Ok(vec![]);
        }
        let bytes = ctx
            .platform
            .dir_size(&artifact)
            .map_err(|e| AdapterError::unavailable(e.to_string()))?;
        if bytes == 0 {
            return Ok(vec![]);
        }
        let operation = match self.tool {
            Tool::Uv => "prune unused uv entries; size is an upper bound",
            Tool::Npm => {
                "clear npm download cache; preserve npx environments and installed packages"
            }
        };
        let candidates = vec![
            Candidate::new(
                artifact.to_string_lossy(),
                format!(
                    "{operation}: {}; {}",
                    artifact.display(),
                    access_label(last_activity)
                ),
                bytes,
                last_activity,
                Class::Cache,
            )
            .confirm(self.cfg.confirm),
        ];
        record_plan(ctx, self.name(), &candidates);
        Ok(candidates)
    }

    fn execute(
        &mut self,
        ctx: &mut Ctx,
        batch: &[Candidate],
    ) -> Result<Vec<Outcome>, AdapterError> {
        let cache_root = self.root(ctx.platform)?;
        let artifact = self.tool.artifact(&cache_root);
        let outcomes: Vec<_> = batch
            .iter()
            .map(|c| {
                let skip = |reason: &str| Outcome::Skipped {
                    id: c.id.clone(),
                    reason: reason.into(),
                };
                if Path::new(&c.id) != artifact || !real_path(ctx.platform, &artifact) {
                    return skip("cache path changed or is unsafe");
                }
                let Some(last_activity) = activity(ctx.platform, &artifact) else {
                    return skip("cannot revalidate cache activity");
                };
                if last_activity > c.last_used {
                    return skip("cache accessed or changed since approval was requested");
                }
                let Ok(before) = ctx.platform.dir_size(&artifact) else {
                    return skip("cannot measure cache");
                };
                match ctx.platform.run_command(&self.tool.clean(&cache_root)) {
                    Ok(out) if out.success() => {
                        // Native pruning can retain live entries. Never report the plan's upper bound as reclaimed.
                        let after = match ctx.platform.dir_size(&artifact) {
                            Ok(n) => n,
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
                            Err(_) => before,
                        };
                        Outcome::Removed {
                            id: c.id.clone(),
                            bytes: before.saturating_sub(after),
                        }
                    }
                    Ok(out) => Outcome::Failed {
                        id: c.id.clone(),
                        error: format!(
                            "{} cache command exited {}: {}",
                            self.name(),
                            out.status,
                            out.stderr_string()
                        ),
                    },
                    Err(e) => Outcome::Failed {
                        id: c.id.clone(),
                        error: e.to_string(),
                    },
                }
            })
            .collect();
        record_results(ctx, self.name(), &outcomes);
        Ok(outcomes)
    }
}
