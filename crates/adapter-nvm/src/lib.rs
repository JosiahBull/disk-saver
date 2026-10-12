//! Age nvm downloads and queue inactive Node installations for explicit approval.
//! Access timestamps are filesystem evidence, never execution history.
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use disk_saver_cachedir::{
    downloads::{Downloads, archive_name},
    native::NativeConfig,
    safety::{access_label, real_path, record_plan, record_results},
};
use disk_saver_core::{
    Adapter, AdapterError, AdapterFactory, Candidate, Class, ConfigCx, Ctx, FileKind, Outcome,
    Platform, RetentionPolicy, expand_tilde,
};

mod commands;
mod config;
mod protection;

#[cfg(test)]
use commands::QUERY;
use commands::{UNINSTALL, command, version_name};

pub use config::Config;

fn nvm_root(p: &dyn Platform, configured: Option<&Path>) -> PathBuf {
    configured
        .map(|v| expand_tilde(v, &p.home_dir()))
        .or_else(|| p.env_var("NVM_DIR").map(PathBuf::from))
        .unwrap_or_else(|| p.home_dir().join(".nvm"))
}

fn downloads(p: &dyn Platform, configured: Option<&Path>) -> PathBuf {
    nvm_root(p, configured).join(".cache")
}

/// Build the nvm adapter.
pub fn factory() -> AdapterFactory {
    AdapterFactory::typed("nvm", |cfg: Config, cx: &ConfigCx| {
        let policy = cx.retention(cfg.max_age, cfg.min_age)?;
        let cache = Downloads {
            name: "nvm",
            root: downloads,
            archive: archive_name,
            cfg: NativeConfig {
                max_age: cfg.max_age,
                min_age: cfg.min_age,
                confirm: cfg.confirm,
                cache_dir: cfg.nvm_dir.clone(),
            },
            policy,
        };
        Ok(Nvm { cfg, policy, cache })
    })
}

struct Nvm {
    cfg: Config,
    policy: RetentionPolicy,
    cache: Downloads,
}

impl Adapter for Nvm {
    fn name(&self) -> &'static str {
        "nvm"
    }

    fn observe(&mut self, _: &mut Ctx) -> Result<(), AdapterError> {
        Ok(())
    }

    fn plan(&mut self, ctx: &mut Ctx) -> Result<Vec<Candidate>, AdapterError> {
        let mut out = self.cache.plan(ctx)?;
        let cached_count = out.len();
        if !self.cfg.include_versions
            || matches!(ctx.pressure, disk_saver_core::Pressure::Comfortable)
        {
            return Ok(out);
        }
        let root = nvm_root(ctx.platform, self.cfg.nvm_dir.as_deref());
        if !real_path(ctx.platform, &root) {
            return Ok(out);
        }
        let versions = root.join("versions/node");
        if !real_path(ctx.platform, &versions) {
            return Ok(out);
        }
        let protected = self.protected(ctx.platform, &root)?;
        for e in ctx
            .platform
            .read_dir(&versions)
            .map_err(|err| AdapterError::unavailable(err.to_string()))?
        {
            if e.kind != FileKind::Dir
                || !version_name(&e.file_name)
                || protected.contains(&e.file_name)
            {
                continue;
            }
            let Some((access, last, size)) = self.evidence(ctx.platform, &e.path) else {
                continue;
            };
            if size > 0
                && self.policy.eligible(
                    ctx.now().duration_since(last).unwrap_or_default(),
                    ctx.pressure,
                )
            {
                let label = format!(
                    "uninstall Node {} including its global packages: {}; {}; project pins checked only under {:?}",
                    e.file_name,
                    e.path.display(),
                    access_label(access),
                    self.cfg.roots,
                );
                out.push(
                    Candidate::new(e.path.to_string_lossy(), label, size, last, Class::UserData)
                        .confirm(true),
                );
            }
        }
        record_plan(ctx, self.name(), &out[cached_count..]);
        Ok(out)
    }

    fn execute(
        &mut self,
        ctx: &mut Ctx,
        batch: &[Candidate],
    ) -> Result<Vec<Outcome>, AdapterError> {
        let root = nvm_root(ctx.platform, self.cfg.nvm_dir.as_deref());
        let versions = root.join("versions/node");
        let mut out = vec![];
        for c in batch {
            let path = Path::new(&c.id);
            if path.starts_with(root.join(".cache")) {
                out.extend(self.cache.execute(ctx, std::slice::from_ref(c))?);
                continue;
            }
            let skip = |reason: &str| Outcome::Skipped {
                id: c.id.clone(),
                reason: reason.into(),
            };
            let version = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            if !self.cfg.include_versions
                || path.parent() != Some(versions.as_path())
                || !version_name(version)
                || !real_path(ctx.platform, path)
                || !c.requires_confirmation
            {
                out.push(skip("not an approved nvm installation"));
                continue;
            }
            if self.protected(ctx.platform, &root)?.contains(version) {
                out.push(skip(
                    "version is now current, aliased, protected, or project-pinned",
                ));
                continue;
            }
            let Some((_, last, before)) = self.evidence(ctx.platform, path) else {
                out.push(skip("installation may be in use or activity is unknown"));
                continue;
            };
            if last > c.last_used || before != c.bytes {
                out.push(skip("installation accessed or changed since planning"));
                continue;
            }
            match ctx
                .platform
                .run_command(&command(&root, UNINSTALL, version))
            {
                Ok(o) if o.success() => {
                    let after = match ctx.platform.dir_size(path) {
                        Ok(n) => n,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
                        Err(_) => before,
                    };
                    out.push(Outcome::Removed {
                        id: c.id.clone(),
                        bytes: before.saturating_sub(after),
                    });
                }
                Ok(o) => out.push(Outcome::Failed {
                    id: c.id.clone(),
                    error: o.stderr_string(),
                }),
                Err(e) => out.push(Outcome::Failed {
                    id: c.id.clone(),
                    error: e.to_string(),
                }),
            }
        }
        let versions_only: Vec<_> = out
            .iter()
            .filter(|o| Path::new(o.id()).parent() == Some(versions.as_path()))
            .cloned()
            .collect();
        record_results(ctx, self.name(), &versions_only);
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
