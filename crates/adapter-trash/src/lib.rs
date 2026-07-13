//! `disk-saver-adapter-trash` — the OS recycle-bin adapter (`ARCHITECTURE.md`
//! §12.5).
//!
//! This is the cross-platform trash cleaner and the first confirmation-gated
//! adapter. All OS divergence hides behind [`Platform::trash_dirs`]; the adapter
//! itself has zero `#[cfg(target_os)]`.
//!
//! Two on-disk layouts are supported, detected per trash directory by the
//! presence of an `info/` sibling directory:
//!
//! * **XDG** (Linux, `~/.local/share/Trash`): entries live in `files/*` and each
//!   carries an `info/<name>.trashinfo` sidecar with a `DeletionDate`, so ages
//!   are exact. Deletion removes `files/X` and `info/X.trashinfo` together.
//! * **Plain** (macOS, `~/.Trash`): deletion dates are not exposed, so age is
//!   tracked via KV — the first time an entry is seen it is stamped
//!   `first_seen = now`, and its age is `now - first_seen` (a strict lower
//!   bound). First-sighting grace applies (§11.5): an entry is never deleted in
//!   the run that discovered it.
//!
//! Everything the adapter deletes is [`Class::UserData`], and by default
//! `confirm = true`, so candidates are flagged `requires_confirmation` and
//! routed to the approvals queue by the engine rather than auto-deleted.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use disk_saver_core::{
    Adapter, AdapterError, AdapterFactory, Candidate, Class, ConfigError, Ctx, Decision,
    DecisionKind, DecisionLog, FileKind, Outcome, Platform, RetentionPolicy, parse_adapter_config,
};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

/// The adapter's stable machine name (config section, KV bucket, log target).
const NAME: &str = "trash";

/// One day, in seconds.
const DAY: u64 = 24 * 60 * 60;

// ── configuration ────────────────────────────────────────────────────────────

fn default_max_age() -> Duration {
    Duration::from_secs(60 * DAY)
}
fn default_min_age() -> Duration {
    Duration::from_secs(7 * DAY)
}
fn default_confirm() -> bool {
    true
}

/// The `[adapters.trash]` config table.
///
/// `confirm` defaults to `true` (trash is [`Class::UserData`]); set it to
/// `false` to restore fully-automatic emptying. `protect` is a list of glob
/// patterns matched against each entry's name; matching entries are never
/// proposed for deletion.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct TrashConfig {
    #[serde(with = "humantime_serde")]
    max_age: Duration,
    #[serde(with = "humantime_serde")]
    min_age: Duration,
    confirm: bool,
    protect: Vec<String>,
}

impl Default for TrashConfig {
    fn default() -> Self {
        Self {
            max_age: default_max_age(),
            min_age: default_min_age(),
            confirm: default_confirm(),
            protect: Vec::new(),
        }
    }
}

// ── KV record ─────────────────────────────────────────────────────────────────

/// Per-entry state for the plain (macOS) layout: when we first saw the item.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct EntryRecord {
    /// Unix seconds at first sighting.
    first_seen: u64,
}

/// The KV key prefix for plain-layout entry records.
const ENTRY_PREFIX: &str = "entry:";

// ── the adapter ────────────────────────────────────────────────────────────────

/// The trash adapter. Built by [`factory`] from the `[adapters.trash]` table.
struct TrashAdapter {
    policy: RetentionPolicy,
    confirm: bool,
    protect: GlobSet,
}

/// Which on-disk trash layout an entry belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// Linux XDG: `files/` + `info/<name>.trashinfo`.
    Xdg,
    /// macOS/plain: entries directly under the trash root, no sidecars.
    Plain,
}

/// A raw trash entry, before age/eligibility resolution.
struct RawEntry {
    /// Absolute path of the entry (doubles as the candidate id).
    path: PathBuf,
    /// The entry's file name.
    name: String,
    /// The layout this entry belongs to.
    layout: Layout,
    /// Reclaimable size in bytes.
    size: u64,
    /// The parsed XDG `DeletionDate`, if this is an XDG entry with a readable,
    /// parseable sidecar. `None` for plain entries or unparseable sidecars.
    xdg_date: Option<SystemTime>,
}

/// Build the trash adapter from its opaque config table.
pub fn factory() -> AdapterFactory {
    AdapterFactory { name: NAME, build }
}

/// The factory `build` function (a plain fn so it coerces to a fn pointer).
fn build(raw: Option<toml::Value>) -> Result<Box<dyn Adapter>, ConfigError> {
    let cfg: TrashConfig = parse_adapter_config(NAME, raw)?;

    let policy = RetentionPolicy::new(cfg.max_age, cfg.min_age);
    policy.validate().map_err(|message| ConfigError::Adapter {
        adapter: NAME.to_owned(),
        message,
    })?;

    let mut builder = GlobSetBuilder::new();
    for pattern in &cfg.protect {
        let glob = Glob::new(pattern).map_err(|e| ConfigError::Adapter {
            adapter: NAME.to_owned(),
            message: format!("invalid protect glob '{pattern}': {e}"),
        })?;
        builder.add(glob);
    }
    let protect = builder.build().map_err(|e| ConfigError::Adapter {
        adapter: NAME.to_owned(),
        message: format!("building protect glob set: {e}"),
    })?;

    Ok(Box::new(TrashAdapter {
        policy,
        confirm: cfg.confirm,
        protect,
    }))
}

impl Adapter for TrashAdapter {
    fn name(&self) -> &'static str {
        NAME
    }

    fn check(&mut self, ctx: &mut Ctx) -> Result<(), AdapterError> {
        // A cheap readability probe for `disk-saver doctor`: scanning surfaces an
        // unreadable trash directory as `Unavailable`.
        scan_raw(ctx.platform).map(|_| ())
    }

    fn observe(&mut self, ctx: &mut Ctx) -> Result<(), AdapterError> {
        let entries = scan_raw(ctx.platform)?;
        let now = ctx.now();
        let now_unix = to_unix(now);

        // Reconcile KV first-seen stamps for plain-layout entries only.
        let mut plain_names: BTreeSet<String> = BTreeSet::new();
        for e in &entries {
            if e.layout != Layout::Plain {
                continue;
            }
            plain_names.insert(e.name.clone());

            let key = entry_key(&e.name);
            let existing: Option<EntryRecord> = ctx.kv.get(&key).map_err(failed)?;
            if existing.is_none() {
                // First sighting: stamp `first_seen = now` (grace applies until
                // the item ages through the policy window).
                ctx.kv
                    .set(
                        &key,
                        &EntryRecord {
                            first_seen: now_unix,
                        },
                        now,
                    )
                    .map_err(failed)?;
            }
            if DecisionLog::compiled() {
                ctx.decide(
                    Decision::new(DecisionKind::Observed, NAME, e.path.to_string_lossy())
                        .label(e.name.clone())
                        .bytes(e.size),
                );
            }
        }

        // GC vanished plain entries: drop KV records whose entry is gone.
        for key in ctx.kv.keys(ENTRY_PREFIX).map_err(failed)? {
            let name = key.strip_prefix(ENTRY_PREFIX).unwrap_or(&key);
            if !plain_names.contains(name) {
                ctx.kv.delete(&key).map_err(failed)?;
            }
        }

        Ok(())
    }

    fn plan(&mut self, ctx: &mut Ctx) -> Result<Vec<Candidate>, AdapterError> {
        let entries = scan_raw(ctx.platform)?;
        let now = ctx.now();
        let mut candidates = Vec::new();

        for e in entries {
            if self.protect.is_match(&e.name) {
                if DecisionLog::compiled() {
                    ctx.decide(
                        Decision::new(DecisionKind::Kept, NAME, e.path.to_string_lossy())
                            .label(e.name.clone())
                            .bytes(e.size)
                            .reason("protected by glob"),
                    );
                }
                continue;
            }

            // Resolve last-used: XDG uses the exact deletion date; plain uses the
            // KV first-seen stamp (defaulting to `now`, which yields age 0 and so
            // enforces first-sighting grace when `observe` has not stamped it).
            let last_used = match e.layout {
                Layout::Xdg => e.xdg_date.unwrap_or(now),
                Layout::Plain => self.first_seen(ctx, &e.name)?.unwrap_or(now),
            };
            let age = now.duration_since(last_used).unwrap_or(Duration::ZERO);

            if self.policy.eligible(age, ctx.pressure) {
                let candidate = Candidate::new(
                    e.path.to_string_lossy().into_owned(),
                    e.name.clone(),
                    e.size,
                    last_used,
                    Class::UserData,
                )
                .confirm(self.confirm);
                if DecisionLog::compiled() {
                    ctx.decide(
                        Decision::new(DecisionKind::Planned, NAME, candidate.id.clone())
                            .label(e.name.clone())
                            .bytes(e.size)
                            .age(age),
                    );
                }
                candidates.push(candidate);
            } else if DecisionLog::compiled() {
                ctx.decide(
                    Decision::new(DecisionKind::Kept, NAME, e.path.to_string_lossy())
                        .label(e.name.clone())
                        .bytes(e.size)
                        .age(age)
                        .reason(format!("age {}d below threshold", age.as_secs() / DAY)),
                );
            }
        }

        Ok(candidates)
    }

    fn execute(
        &mut self,
        ctx: &mut Ctx,
        batch: &[Candidate],
    ) -> Result<Vec<Outcome>, AdapterError> {
        let mut outcomes = Vec::with_capacity(batch.len());

        for cand in batch {
            let entry = PathBuf::from(&cand.id);
            let name = entry
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();

            // Marker re-validation: the entry may have vanished (emptied by the
            // user, re-observed and GC'd) between plan and execute. Skip if gone.
            let meta = match ctx.platform.metadata(&entry) {
                Ok(m) => m,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    let reason = "entry no longer in trash".to_owned();
                    if DecisionLog::compiled() {
                        ctx.decide(
                            Decision::new(DecisionKind::Skipped, NAME, cand.id.clone())
                                .label(cand.label.clone())
                                .reason(reason.clone()),
                        );
                    }
                    outcomes.push(Outcome::Skipped {
                        id: cand.id.clone(),
                        reason,
                    });
                    continue;
                }
                Err(e) => {
                    let reason = format!("cannot stat trash entry: {e}");
                    if DecisionLog::compiled() {
                        ctx.decide(
                            Decision::new(DecisionKind::Skipped, NAME, cand.id.clone())
                                .label(cand.label.clone())
                                .reason(reason.clone()),
                        );
                    }
                    outcomes.push(Outcome::Skipped {
                        id: cand.id.clone(),
                        reason,
                    });
                    continue;
                }
            };

            // An XDG entry has a readable `info/<name>.trashinfo` sidecar; a plain
            // entry does not (and is tracked in KV instead).
            let info = info_sidecar(&entry).filter(|p| ctx.platform.metadata(p).is_ok());

            let removal = match meta.kind {
                FileKind::Dir => ctx.platform.remove_dir_all(&entry),
                // File, Symlink, Other → unlink directly (never traverse).
                _ => ctx.platform.remove_file(&entry),
            };

            match removal {
                Ok(()) => {
                    match &info {
                        // XDG: remove the sidecar together with the entry.
                        Some(info_path) => {
                            let _ = ctx.platform.remove_file(info_path);
                        }
                        // Plain: drop the KV first-seen record.
                        None => {
                            let _ = ctx.kv.delete(&entry_key(&name));
                        }
                    }
                    if DecisionLog::compiled() {
                        ctx.decide(
                            Decision::new(DecisionKind::Deleted, NAME, cand.id.clone())
                                .label(cand.label.clone())
                                .bytes(cand.bytes),
                        );
                    }
                    outcomes.push(Outcome::Removed {
                        id: cand.id.clone(),
                        bytes: cand.bytes,
                    });
                }
                Err(e) => {
                    let error = e.to_string();
                    if DecisionLog::compiled() {
                        ctx.decide(
                            Decision::new(DecisionKind::Failed, NAME, cand.id.clone())
                                .label(cand.label.clone())
                                .reason(error.clone()),
                        );
                    }
                    outcomes.push(Outcome::Failed {
                        id: cand.id.clone(),
                        error,
                    });
                }
            }
        }

        Ok(outcomes)
    }
}

impl TrashAdapter {
    /// Read the KV first-seen stamp for a plain-layout entry, if present.
    fn first_seen(&self, ctx: &Ctx, name: &str) -> Result<Option<SystemTime>, AdapterError> {
        let rec: Option<EntryRecord> = ctx.kv.get(&entry_key(name)).map_err(failed)?;
        Ok(rec.map(|r| UNIX_EPOCH + Duration::from_secs(r.first_seen)))
    }
}

// ── scanning ─────────────────────────────────────────────────────────────────

/// Enumerate every top-level entry across all trash directories.
///
/// A missing trash directory (or a missing `files/` under an XDG root) is treated
/// as an empty trash, not an error. Any other read failure — most importantly a
/// `PermissionDenied` (macOS Full Disk Access not granted) — is surfaced as
/// [`AdapterError::Unavailable`] so the adapter is skipped and retried next run.
fn scan_raw(platform: &dyn Platform) -> Result<Vec<RawEntry>, AdapterError> {
    let mut out = Vec::new();

    for trash in platform.trash_dirs() {
        let info_dir = trash.join("info");
        let is_xdg = matches!(platform.metadata(&info_dir), Ok(m) if m.kind == FileKind::Dir);

        if is_xdg {
            let files_dir = trash.join("files");
            let entries = match platform.read_dir(&files_dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(read_failure(&files_dir, &e)),
            };
            for de in entries {
                let xdg_date = read_deletion_date(platform, &trash, &de.file_name);
                let size = entry_size(platform, &de.path, de.kind);
                out.push(RawEntry {
                    path: de.path,
                    name: de.file_name,
                    layout: Layout::Xdg,
                    size,
                    xdg_date,
                });
            }
        } else {
            let entries = match platform.read_dir(&trash) {
                Ok(entries) => entries,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(read_failure(&trash, &e)),
            };
            for de in entries {
                let size = entry_size(platform, &de.path, de.kind);
                out.push(RawEntry {
                    path: de.path,
                    name: de.file_name,
                    layout: Layout::Plain,
                    size,
                    xdg_date: None,
                });
            }
        }
    }

    Ok(out)
}

/// Map a trash-directory read failure to [`AdapterError::Unavailable`], adding a
/// Full Disk Access hint when the cause is a permission error.
fn read_failure(dir: &Path, e: &io::Error) -> AdapterError {
    if e.kind() == io::ErrorKind::PermissionDenied {
        AdapterError::unavailable(format!(
            "cannot read trash directory {}: {e} — on macOS, grant Full Disk Access to \
             disk-saver (see `disk-saver doctor`)",
            dir.display()
        ))
    } else {
        AdapterError::unavailable(format!(
            "cannot read trash directory {}: {e}",
            dir.display()
        ))
    }
}

/// Reclaimable size of a trash entry, by kind (directories recurse; symlinks and
/// other kinds contribute nothing beyond their own inode).
fn entry_size(platform: &dyn Platform, path: &Path, kind: FileKind) -> u64 {
    match kind {
        FileKind::Dir => platform.dir_size(path).unwrap_or(0),
        FileKind::File => platform.metadata(path).map(|m| m.len).unwrap_or(0),
        FileKind::Symlink | FileKind::Other => 0,
    }
}

/// The `info/<name>.trashinfo` sidecar path for an XDG entry under `files/`, if
/// `entry` has the `<trash>/files/<name>` shape; otherwise `None` (plain layout).
fn info_sidecar(entry: &Path) -> Option<PathBuf> {
    let files_dir = entry.parent()?;
    if files_dir.file_name()? != "files" {
        return None;
    }
    let trash_root = files_dir.parent()?;
    let name = entry.file_name()?;
    let mut file = name.to_os_string();
    file.push(".trashinfo");
    Some(trash_root.join("info").join(file))
}

/// Read and parse the `DeletionDate` from an XDG `info/<name>.trashinfo` sidecar.
fn read_deletion_date(platform: &dyn Platform, trash: &Path, name: &str) -> Option<SystemTime> {
    let info_path = trash.join("info").join(format!("{name}.trashinfo"));
    let text = platform.read_to_string(&info_path).ok()?;
    for line in text.lines() {
        if let Some(value) = line.trim().strip_prefix("DeletionDate=") {
            return parse_deletion_date(value.trim());
        }
    }
    None
}

/// Parse an XDG `DeletionDate` (`YYYY-MM-DDThh:mm:ss`, local time) into a
/// [`SystemTime`]. The value is interpreted against UTC (no timezone database is
/// available); since ages are computed against the same clock this is consistent.
fn parse_deletion_date(s: &str) -> Option<SystemTime> {
    // Tolerate a trailing `Z` and fractional seconds.
    let s = s.trim().trim_end_matches('Z');
    let (date, time) = s.split_once('T')?;

    let mut d = date.split('-');
    let year: i64 = d.next()?.trim().parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    if d.next().is_some() {
        return None;
    }

    let time = time.split('.').next().unwrap_or(time);
    let mut t = time.split(':');
    let hh: i64 = t.next()?.parse().ok()?;
    let mm: i64 = t.next()?.parse().ok()?;
    let ss: i64 = t.next().unwrap_or("0").parse().ok()?;
    if t.next().is_some() {
        return None;
    }

    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..24).contains(&hh)
        || !(0..60).contains(&mm)
        || !(0..=60).contains(&ss)
    {
        return None;
    }

    let days = days_from_civil(year, month, day);
    let secs = days
        .checked_mul(86_400)?
        .checked_add(hh * 3_600 + mm * 60 + ss)?;
    let secs: u64 = secs.try_into().ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

/// Days since the Unix epoch for a proleptic-Gregorian civil date
/// (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

// ── small helpers ─────────────────────────────────────────────────────────────

/// The KV key for a plain-layout entry's first-seen record.
fn entry_key(name: &str) -> String {
    format!("{ENTRY_PREFIX}{name}")
}

/// Unix seconds for a [`SystemTime`], saturating pre-epoch times to zero.
fn to_unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Wrap any std error as [`AdapterError::Failed`] (used for KV errors).
fn failed(e: impl std::error::Error + Send + Sync + 'static) -> AdapterError {
    AdapterError::Failed(anyhow::Error::new(e))
}

#[cfg(test)]
mod tests;
