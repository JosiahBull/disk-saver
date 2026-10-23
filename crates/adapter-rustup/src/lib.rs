//! `disk-saver-adapter-rustup` — the `rustup` adapter: retire installed Rust
//! toolchains that nothing needs any more, with approval.
//!
//! Every bump of a project's `rust-toolchain` pin leaves the previous toolchain
//! installed, at about 1.2 GB each, and rustup never removes one by itself.
//! They pile up quietly: seven stale toolchains came to 7 GB on the machine
//! this was written for. Each is a [`Class::Cache`]: rustup downloads it again
//! the next time anything asks for it, and it holds no user data — binaries
//! from `cargo install` live in `~/.cargo/bin`, not in a toolchain.
//!
//! # What is never offered
//!
//! * The default toolchain (`default_toolchain` in rustup's `settings.toml`,
//!   and any toolchain `rustup show` flags as default).
//! * `stable-<host>`, `beta-<host>` and `nightly-<host>`. Each follows a
//!   channel and `rustup update` replaces it in place, so none is ever the old
//!   toolchain a pin bump leaves behind.
//! * A toolchain named by a `rustup override` whose directory still exists.
//! * A toolchain pinned by a `rust-toolchain` or `rust-toolchain.toml` in a
//!   project under the configured roots whose last activity is within
//!   [`pin_max_idle`](Config::pin_max_idle) (30 days by default). A pin only
//!   protects while its project is in use: a project nobody has touched for a
//!   month will not need its toolchain soon, and if it does, rustup reinstalls
//!   it. Idle pins are named in the approval label instead.
//! * A toolchain made with `rustup toolchain link` (a symlink to someone's own
//!   build), and any directory whose name is not shaped like a rustup
//!   toolchain.
//!
//! If a pin in an *active* project cannot be read or parsed, it might name any
//! toolchain, so the protection set is incomplete: the run proposes nothing,
//! records why against each toolchain it would have considered, and reports
//! the adapter unavailable until the pin is fixed.
//!
//! Pins are found by the shared walk. A project's `.disk-saver-keep` does not
//! hide its pin (the sentinel exempts a project from deletion, not from
//! protecting what it names), and a symlinked pin is read through the link. The
//! walk does not look inside dot-directories, `exclude`d subtrees or other
//! filesystems, nor below [`max_depth`](Config::max_depth) — raise it to reach
//! deeper projects. A pin there protects nothing. The depth limit is shared by
//! every walk and is hit routinely in trees no project lives in (vendored Helm
//! charts), so reaching it is not treated as a gap.
//!
//! A directory the walk *could not read* is a gap: a scheduled job without
//! Full Disk Access cannot read `~/Documents`, and a pin there went unseen.
//! The run still proposes, since refusing would stop the adapter for good on
//! such a machine, but every candidate that run needs approval whatever
//! `confirm` says, and its label ends `pin scan incomplete: 3 folders
//! unreadable (e.g. ~/Documents)`.
//!
//! # Evidence of use
//!
//! rustup's proxies in `~/.cargo/bin` exec the real binaries in
//! `<toolchain>/bin`, which records an access time on them where the
//! filesystem keeps one. A toolchain's last use is the newest access or
//! modification of those files, raised by the newest file access or
//! modification anywhere in its tree (as the `nvm` adapter does). It is
//! filesystem evidence, not execution history. An unknown access time, an
//! unreadable tree, or an executable that is open in a running process (per
//! `lsof`, which must be present) refuses retirement rather than guessing.
//!
//! A toolchain built for another target goes through the same age policy — a
//! Linux container sharing `~/.rustup` through a mount installs and runs those
//! — but its label names the target, and says when this machine cannot run it
//! at all.
//!
//! # Phases
//!
//! * **observe** — nothing: all evidence is on the filesystem.
//! * **plan** — under [`Comfortable`](Pressure::Comfortable) pressure, nothing:
//!   no rustup, no walk. Otherwise resolve rustup (see
//!   [`Config::rustup`]; never via `PATH`), read the host from `rustup show`,
//!   protect as above, and screen each remaining toolchain on its `bin`
//!   evidence. Only when one is old enough are the roots walked for pins and
//!   its whole tree read. Candidates require confirmation unless `confirm = false`.
//! * **execute** — repeat the protection walk and the evidence, skip a
//!   toolchain that is now protected, was used or changed since planning, or
//!   is running — or that was not approved by a person while the walk is now
//!   incomplete — then `rustup toolchain uninstall <name>` and report the size
//!   reduction actually measured.
//!
//! Labels carry dates, not "4 months ago", so an item reads the same from one
//! run to the next: `uninstall Rust 1.93.1-aarch64-apple-darwin: last used
//! 2026-04-12; pinned only by ~/work/app (last edited 2026-07-01)`.
#![forbid(unsafe_code)]

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use disk_saver_cachedir::safety::real_path;
use disk_saver_core::{
    Adapter, AdapterError, AdapterFactory, Candidate, Class, ConfigCx, ConfigError, Ctx, Decision,
    DecisionKind, DecisionLog, Outcome, Platform, Pressure, RetentionPolicy,
};
use disk_saver_scan::FsConfig;

mod commands;
mod config;
mod label;
mod protection;
mod toolchain;

pub use config::Config;

use commands::{rustup_binary, rustup_home};
use protection::Protection;
use toolchain::{Bin, Installed, Toolchain, bin_activity, evidence};

/// The stable adapter name (config section, KV bucket, log target).
const NAME: &str = "rustup";

/// Re-downloadable, holds no user data.
const CLASS: Class = Class::Cache;

/// Why a toolchain that is otherwise old enough stays: `lsof` either saw one
/// of its executables open, or could not run to say it did not.
const IN_USE: &str =
    "one of its executables is open in a running process, or `lsof` could not rule that out";

/// Build the `rustup` adapter.
pub fn factory() -> AdapterFactory {
    AdapterFactory::typed(NAME, build)
}

/// Validate the age thresholds, the exclude globs and the rustup path.
fn build(cfg: Config, cx: &ConfigCx) -> Result<Rustup, ConfigError> {
    let policy = cx.retention(cfg.max_age, cfg.min_age)?;
    cx.globs("exclude", &cfg.exclude)?;
    if let Some(path) = &cfg.rustup
        && !path.is_absolute()
        && !path.starts_with("~")
    {
        return Err(cx.err(format!(
            "rustup = {path:?} must be an absolute path or start with ~"
        )));
    }
    Ok(Rustup { cfg, policy })
}

struct Rustup {
    cfg: Config,
    policy: RetentionPolicy,
}

/// What one look at rustup found: everything except the pin walk.
struct Survey {
    rustup: PathBuf,
    /// `<rustup home>/toolchains`.
    dir: PathBuf,
    host: String,
    installed: Installed,
    protection: Protection,
}

impl Rustup {
    /// Resolve rustup, its toolchains, and every protection but pins. `None`
    /// when rustup has no toolchains directory, so there is nothing to retire.
    fn survey(&self, p: &dyn Platform) -> Result<Option<Survey>, AdapterError> {
        let rustup = rustup_binary(p, self.cfg.rustup.as_deref())?;
        let home = rustup_home(p);
        let dir = home.join("toolchains");
        match p.metadata(&dir) {
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(AdapterError::unavailable(format!(
                    "cannot inspect {}: {e}",
                    dir.display()
                )));
            }
            Ok(_) if !real_path(p, &dir) => {
                return Err(AdapterError::unavailable(format!(
                    "{} is not a real directory (a symlink in its path?)",
                    dir.display()
                )));
            }
            Ok(_) => {}
        }
        let show = commands::show(p, &rustup)?;
        // `toolchain uninstall` acts on rustup's home; refuse if that is not
        // the directory this adapter inspected.
        if let Some(reported) = &show.home
            && *reported != home
        {
            return Err(AdapterError::unavailable(format!(
                "rustup reports its home as {} but disk-saver resolved {}",
                reported.display(),
                home.display()
            )));
        }
        let installed = toolchain::installed(p, &dir)?;
        let protection = Protection::base(p, &home, &show, &installed.names)?;
        Ok(Some(Survey {
            rustup,
            dir,
            host: show.host,
            installed,
            protection,
        }))
    }

    /// Add project pins to the survey's protection set and return the
    /// directories the walk could not read. `Err` says which pin in an
    /// active project could not be read, in which case nothing may be retired.
    fn scan_pins(
        &self,
        p: &dyn Platform,
        now: SystemTime,
        s: &mut Survey,
    ) -> Result<Vec<PathBuf>, String> {
        let fs = FsConfig {
            roots: self.cfg.roots.clone(),
            exclude: self.cfg.exclude.clone(),
            max_depth: self.cfg.max_depth,
            ..FsConfig::default()
        };
        let (roots, opts) = fs
            .resolve_walk(p)
            .map_err(|e| format!("cannot compile the exclude globs: {e}"))?;
        let gaps = s
            .protection
            .add_pins(p, &roots, &opts, now, self.cfg.pin_max_idle);
        match gaps.broken.as_slice() {
            [] => Ok(gaps.unreadable),
            [one] => Err(format!(
                "a toolchain pin in an active project could not be read ({one}), \
                 so which toolchains are pinned is unknown"
            )),
            [first, rest @ ..] => Err(format!(
                "toolchain pins in active projects could not be read ({first}, and {} more), \
                 so which toolchains are pinned is unknown",
                rest.len()
            )),
        }
    }

    /// Re-check one approved candidate against a fresh survey, then uninstall.
    fn retire(&self, p: &dyn Platform, s: &Survey, c: &Candidate) -> Outcome {
        let skip = |reason: String| Outcome::Skipped {
            id: c.id.clone(),
            reason,
        };
        let Some(tc) = s
            .installed
            .toolchains
            .iter()
            .find(|t| t.path.as_path() == Path::new(&c.id))
        else {
            return skip("not an installed rustup toolchain".into());
        };
        if let Some(why) = s.protection.reason(&tc.name) {
            return skip(format!("now protected: {why}"));
        }
        let bin = match bin_activity(p, &tc.path) {
            Ok(bin) => bin,
            Err(why) => return skip(why),
        };
        let ev = match evidence(p, &tc.path, &bin) {
            Ok(ev) => ev,
            Err(why) => return skip(why),
        };
        if ev.last > c.last_used || ev.bytes != c.bytes {
            return skip("the toolchain was used or changed since it was planned".into());
        }
        if !commands::idle(p, &bin.files) {
            return skip(IN_USE.into());
        }
        match p.run_command(&commands::uninstall(&s.rustup, &tc.name)) {
            Ok(out) if out.success() => {
                let after = match p.dir_size(&tc.path) {
                    Ok(n) => n,
                    Err(e) if e.kind() == ErrorKind::NotFound => 0,
                    // Claim nothing that was not measured.
                    Err(_) => ev.bytes,
                };
                Outcome::Removed {
                    id: c.id.clone(),
                    bytes: ev.bytes.saturating_sub(after),
                }
            }
            Ok(out) => Outcome::Failed {
                id: c.id.clone(),
                error: format!(
                    "`rustup toolchain uninstall` exited {}: {}",
                    out.status,
                    out.stderr_string().trim()
                ),
            },
            Err(e) => Outcome::Failed {
                id: c.id.clone(),
                error: e.to_string(),
            },
        }
    }
}

impl Adapter for Rustup {
    fn name(&self) -> &'static str {
        NAME
    }

    /// For `disk-saver doctor`: can rustup be found without `PATH`?
    fn check(&mut self, ctx: &mut Ctx) -> Result<(), AdapterError> {
        rustup_binary(ctx.platform, self.cfg.rustup.as_deref()).map(|_| ())
    }

    fn observe(&mut self, _: &mut Ctx) -> Result<(), AdapterError> {
        Ok(())
    }

    fn plan(&mut self, ctx: &mut Ctx) -> Result<Vec<Candidate>, AdapterError> {
        if matches!(ctx.pressure, Pressure::Comfortable) {
            return Ok(Vec::new());
        }
        let p = ctx.platform;
        let now = ctx.now();
        let Some(mut s) = self.survey(p)? else {
            return Ok(Vec::new());
        };
        // Ours, kept or not: no other adapter should reason about these trees.
        ctx.claim(s.dir.clone());

        // Screen on the cheap `bin` evidence before walking anything.
        let mut idle: Vec<(Toolchain, Bin)> = Vec::new();
        for tc in &s.installed.toolchains {
            if let Some(why) = s.protection.reason(&tc.name) {
                kept(ctx, tc, None, why);
                continue;
            }
            let bin = match bin_activity(p, &tc.path) {
                Ok(bin) => bin,
                Err(why) => {
                    kept(ctx, tc, None, &why);
                    continue;
                }
            };
            let age = elapsed(now, bin.newest);
            if !self.policy.eligible(age, ctx.pressure) {
                kept(ctx, tc, Some(age), &too_recent(bin.newest, ctx.pressure));
                continue;
            }
            idle.push((tc.clone(), bin));
        }
        if idle.is_empty() {
            return Ok(Vec::new());
        }

        let unreadable = match self.scan_pins(p, now, &mut s) {
            Ok(unreadable) => unreadable,
            Err(why) => {
                for (tc, bin) in &idle {
                    kept(ctx, tc, Some(elapsed(now, bin.newest)), &why);
                }
                return Err(AdapterError::unavailable(format!(
                    "{why}; retiring no toolchain this run"
                )));
            }
        };
        // Where the walk could not read, a pin may have gone unseen. Refusing
        // would stop the adapter for good on a machine where the scheduled job
        // cannot read ~/Documents, so instead every candidate asks first and
        // says why.
        let incomplete = label::incomplete(&p.home_dir(), &unreadable);

        let mut out = Vec::new();
        for (tc, bin) in idle {
            if let Some(why) = s.protection.reason(&tc.name) {
                kept(ctx, &tc, Some(elapsed(now, bin.newest)), why);
                continue;
            }
            let ev = match evidence(p, &tc.path, &bin) {
                Ok(ev) => ev,
                Err(why) => {
                    kept(ctx, &tc, Some(elapsed(now, bin.newest)), &why);
                    continue;
                }
            };
            let age = elapsed(now, ev.last);
            if !self.policy.eligible(age, ctx.pressure) {
                kept(ctx, &tc, Some(age), &too_recent(ev.last, ctx.pressure));
                continue;
            }
            if ev.bytes == 0 {
                continue;
            }
            if !commands::idle(p, &bin.files) {
                kept(ctx, &tc, Some(age), IN_USE);
                continue;
            }
            let text = label::candidate(
                &tc,
                &s.host,
                ev.last,
                s.protection.stale_refs(&tc.name),
                incomplete.as_deref(),
            );
            let c = Candidate::new(tc.path.to_string_lossy(), text, ev.bytes, ev.last, CLASS)
                .confirm(self.cfg.confirm || incomplete.is_some());
            if DecisionLog::compiled() {
                ctx.decide(
                    Decision::new(DecisionKind::Planned, NAME, &c.id)
                        .label(&c.label)
                        .bytes(c.bytes)
                        .age(age),
                );
            }
            out.push(c);
        }
        Ok(out)
    }

    fn execute(
        &mut self,
        ctx: &mut Ctx,
        batch: &[Candidate],
    ) -> Result<Vec<Outcome>, AdapterError> {
        let p = ctx.platform;
        // One survey and one pin walk for the whole batch; the walk is the
        // expensive part, and nothing it finds is specific to a candidate.
        let survey = match self.survey(p) {
            Ok(Some(mut s)) => self
                .scan_pins(p, ctx.now(), &mut s)
                .map(|unreadable| (s, label::incomplete(&p.home_dir(), &unreadable))),
            Ok(None) => Err("rustup has no toolchains directory".to_owned()),
            Err(e) => Err(e.to_string()),
        };
        let mut out = Vec::with_capacity(batch.len());
        for c in batch {
            let outcome = match &survey {
                // Planned on a complete scan, but this one is not: only a
                // person may approve it now, as plan would have required.
                Ok((_, Some(note))) if !c.requires_confirmation => Outcome::Skipped {
                    id: c.id.clone(),
                    reason: format!("{note}, so it needs approval"),
                },
                Ok((s, _)) => self.retire(p, s, c),
                Err(why) => Outcome::Skipped {
                    id: c.id.clone(),
                    reason: format!("cannot confirm nothing needs it: {why}"),
                },
            };
            record_outcome(ctx, c, &outcome);
            out.push(outcome);
        }
        Ok(out)
    }
}

fn elapsed(now: SystemTime, then: SystemTime) -> Duration {
    now.duration_since(then).unwrap_or_default()
}

fn too_recent(last_used: SystemTime, pressure: Pressure) -> String {
    format!(
        "last used {}, too recently for {} pressure",
        label::date(last_used),
        pressure.label()
    )
}

/// Record why a toolchain stays, for `disk-saver why <path>`.
fn kept(ctx: &Ctx, tc: &Toolchain, age: Option<Duration>, why: &str) {
    if !DecisionLog::compiled() {
        return;
    }
    let mut d = Decision::new(DecisionKind::Kept, NAME, tc.path.to_string_lossy())
        .label(format!("Rust toolchain {}", tc.name))
        .reason(why);
    if let Some(age) = age {
        d = d.age(age);
    }
    ctx.decide(d);
}

/// Record the result of an execute attempt.
fn record_outcome(ctx: &Ctx, c: &Candidate, outcome: &Outcome) {
    if !DecisionLog::compiled() {
        return;
    }
    let (kind, bytes, reason) = match outcome {
        Outcome::Removed { bytes, .. } => (DecisionKind::Deleted, *bytes, None),
        Outcome::Skipped { reason, .. } => (DecisionKind::Skipped, c.bytes, Some(reason)),
        Outcome::Failed { error, .. } => (DecisionKind::Failed, c.bytes, Some(error)),
    };
    let mut d = Decision::new(kind, NAME, &c.id)
        .label(&c.label)
        .bytes(bytes)
        .age(c.age(ctx.now()));
    if let Some(reason) = reason {
        d = d.reason(reason);
    }
    ctx.decide(d);
}

#[cfg(test)]
mod tests;
