use std::collections::BTreeSet;
use std::path::Path;
use std::time::SystemTime;

use disk_saver_cachedir::safety::{activity, real_path};
use disk_saver_core::{AdapterError, FileKind, Platform, expand_tilde};

use crate::Nvm;
use crate::commands::{QUERY, command, idle, version_name};

impl Nvm {
    pub(super) fn resolve(
        &self,
        p: &dyn Platform,
        root: &Path,
        spec: &str,
    ) -> Result<Option<String>, AdapterError> {
        let out = p
            .run_command(&command(root, QUERY, spec))
            .map_err(|e| AdapterError::unavailable(e.to_string()))?;
        let text = out.stdout_string();
        let value = text.trim();
        if value == "N/A" {
            return Ok(None);
        }
        if !out.success() || !(version_name(value) || value == "system") {
            return Err(AdapterError::unavailable(format!(
                "cannot resolve protected nvm version {spec}"
            )));
        }
        Ok(Some(value.into()))
    }

    pub(super) fn protected(
        &self,
        p: &dyn Platform,
        root: &Path,
    ) -> Result<BTreeSet<String>, AdapterError> {
        let mut specs = BTreeSet::from(["default".into(), "current".into()]);
        specs.extend(self.cfg.protect.iter().cloned());
        // Named aliases can be relied upon by scripts even when not the default.
        let alias = root.join("alias");
        match p.metadata(&alias) {
            Ok(_) if !real_path(p, &alias) => {
                return Err(AdapterError::unavailable("unsafe nvm aliases directory"));
            }
            Ok(_) => {
                for e in p
                    .read_dir(&alias)
                    .map_err(|e| AdapterError::unavailable(e.to_string()))?
                {
                    if e.kind == FileKind::File {
                        specs.insert(e.file_name);
                    } else if e.kind == FileKind::Symlink {
                        return Err(AdapterError::unavailable("symlinked nvm alias"));
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(AdapterError::unavailable(e.to_string())),
        }
        // Pins outside these roots are not claimed to have been checked.
        let mut todo: Vec<_> = self
            .cfg
            .roots
            .iter()
            .map(|r| (expand_tilde(r, &p.home_dir()), 0))
            .collect();
        let mut count = 0;
        while let Some((dir, depth)) = todo.pop() {
            if p.metadata(&dir).is_ok() && !real_path(p, &dir) {
                return Err(AdapterError::unavailable("unsafe project-pin root"));
            }
            let entries = match p.read_dir(&dir) {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(AdapterError::unavailable(format!(
                        "cannot check project pins: {e}"
                    )));
                }
            };
            for e in entries {
                count += 1;
                if count > 50_000 {
                    return Err(AdapterError::unavailable("project-pin scan limit exceeded"));
                }
                if e.file_name == ".nvmrc" || e.file_name == ".node-version" {
                    if e.kind != FileKind::File {
                        return Err(AdapterError::unavailable(
                            "project version pin is not a regular file",
                        ));
                    }
                    let text = p
                        .read_to_string(&e.path)
                        .map_err(|err| AdapterError::unavailable(err.to_string()))?;
                    let spec = text
                        .lines()
                        .map(str::trim)
                        .find(|line| !line.is_empty() && !line.starts_with('#'))
                        .ok_or_else(|| AdapterError::unavailable("empty project version pin"))?;
                    specs.insert(spec.into());
                } else if e.kind == FileKind::Dir
                    && !e.file_name.starts_with('.')
                    && !["node_modules", "target", "Library"].contains(&e.file_name.as_str())
                {
                    if depth >= 8 {
                        return Err(AdapterError::unavailable(
                            "project-pin depth limit exceeded; narrow roots",
                        ));
                    }
                    if real_path(p, &e.path) {
                        todo.push((e.path, depth + 1));
                    }
                }
            }
        }
        let mut protected = BTreeSet::new();
        for spec in specs {
            match self.resolve(p, root, &spec)? {
                Some(v) => {
                    protected.insert(v);
                }
                None if spec == "default" || spec == "current" => {
                    return Err(AdapterError::unavailable(
                        "nvm default/current version is unresolved; refusing retirement",
                    ));
                }
                None => {} // A pin to an uninstalled version cannot protect an installed version.
            }
        }
        Ok(protected)
    }

    pub(super) fn evidence(
        &self,
        p: &dyn Platform,
        path: &Path,
    ) -> Option<(SystemTime, SystemTime, u64)> {
        if !real_path(p, path) {
            return None;
        }
        let node = path.join("bin/node");
        let m = p.metadata(&node).ok()?;
        if !real_path(p, &node) || m.kind != FileKind::File || !idle(p, &node) {
            return None;
        }
        let mut access = m.accessed?;
        // Include every real executable in bin; symlinked npm/npx are covered by node and tree activity.
        for e in p.read_dir(&path.join("bin")).ok()? {
            if e.kind == FileKind::File {
                access = access.max(p.metadata(&e.path).ok()?.accessed?);
            }
        }
        let activity = activity(p, path)?.max(access);
        Some((access, activity, p.dir_size(path).ok()?))
    }
}
