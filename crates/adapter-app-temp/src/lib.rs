//! `disk-saver-adapter-app-temp` — old items in macOS app-sandbox temp folders.
//!
//! Every sandboxed macOS app gets a temp folder of its own,
//! `~/Library/Containers/<bundle-id>/Data/tmp`. Unlike the per-user `$TMPDIR`
//! under `/var/folders`, which macOS sweeps, nothing ever cleans these: an
//! 8.6 GB installer sent through Messages sat in
//! `com.apple.MobileSMS/Data/tmp/TemporaryItems/…` for eight months. This adapter
//! finds what has sat untouched in them and, by default, asks before removing it.
//!
//! Linux has no `~/Library/Containers`, and a missing containers directory is
//! simply nothing to do — no `#[cfg(target_os)]` needed.
//!
//! # Units
//!
//! A temp folder is cleaned in *units*, each aged, sized and removed whole:
//!
//! * every direct child of `tmp`, except
//! * `tmp/TemporaryItems`, a staging directory (Foundation puts its item
//!   replacement directories there): each of *its* children is a unit instead,
//!   except
//! * `tmp/TemporaryItems/<bundle-id>`, named after the container itself: the
//!   app's own staging root. Messages keeps `Media/<UUID>/` and
//!   `LinkedFiles/<UUID>/` there, one directory per attachment, so each child of
//!   each directory in it is a unit.
//!
//! Without that last rule Messages' whole staging tree would be one unit that
//! never ages, because an attachment arrives most days, and the eight-month-old
//! installer would never qualify. Staging directories themselves are never
//! removed, only what is in them. Symlinks are never followed: a symlinked
//! container, `Data`, `tmp` or unit is left alone.
//!
//! # Age
//!
//! A unit's activity is the newest modification or access time of anything in
//! it, per [`disk_saver_cachedir::safety::activity`] (directory access times
//! excluded: listing a directory updates them). So a unit is old only if nothing
//! inside it is newer than the cutoff. A unit whose activity cannot be
//! established — unreadable, holding a nested mount or a socket, too large to
//! walk — is never proposed.
//!
//! # Candidates
//!
//! One per container that has old units, so that `disk-saver review` asks about
//! `com.apple.geod`'s thousand cached files once rather than a thousand times:
//!
//! * **id** — the temp folder, `<container>/Data/tmp`;
//! * **label** — `4 old items in com.apple.MobileSMS temp (oldest 8 months,
//!   newest 31 days): Some Installer-1.0.pkg 8.6 GB, …`, naming the largest;
//! * **bytes** — the old units' combined size;
//! * **last_used** — the newest activity among them, so the group is never
//!   treated as older than its youngest member;
//! * [`Class::UserData`] — a temp file can be someone's only copy of a download;
//! * `requires_confirmation` from `confirm`, which defaults to `true`.
//!
//! All the walking happens in `plan`, and `plan` returns at once when pressure is
//! comfortable. `observe` does nothing. Every look inside another app's
//! container is an access attempt that macOS privacy protection may log or
//! refuse, so a comfortable run makes none.
//!
//! # Execution
//!
//! `execute` recomputes the container's units by the same rules and removes only
//! those whose activity is no newer than the candidate's `last_used`. Anything
//! touched since approval, or created since with a recent timestamp, stays. The
//! approved size is a ceiling: if the old units now add up to more than was
//! approved (something arrived carrying an old timestamp), the whole candidate is
//! skipped and comes back for review on the next run with a fresh label.
//!
//! Units are independent, so one failing does not stop the rest. The candidate's
//! [`Outcome`] is [`Outcome::Removed`] with the bytes actually freed when every
//! removal succeeded, and [`Outcome::Failed`] if any did not. The error says how
//! much was removed and what was left, so a partial clean is neither hidden nor
//! mistaken for nothing happening. What remains is proposed again on the next
//! run. Each removed or failed unit is also recorded under its own path, so
//! `disk-saver why` can answer for a single file.
//!
//! # Permissions
//!
//! Reading other apps' containers from a launchd job can be refused by macOS
//! privacy protection. If the containers directory itself cannot be listed, or
//! every temp folder in it is refused, the adapter reports
//! [`AdapterError::Unavailable`] with a hint to grant Full Disk Access, the same
//! as `trash`. A single unreadable container is skipped and recorded as kept.
//!
//! # Config (`[adapters.app-temp]`)
//!
//! | key | default | meaning |
//! |---|---|---|
//! | `max_age` | `"30d"` | normal mode: propose units with no activity for this long |
//! | `min_age` | `"7d"` | scavenge floor: never propose anything newer |
//! | `confirm` | `true` | queue for `disk-saver review` rather than delete unattended |
//! | `exclude_bundles` | `[]` | container folder names (bundle ids) to leave alone |
//! | `containers_dir` | `"~/Library/Containers"` | where the app containers live |

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use disk_saver_cachedir::safety::{activity, real_path, record_plan, record_results};
use disk_saver_core::{
    Adapter, AdapterError, AdapterFactory, Candidate, Class, ConfigCx, ConfigError, Ctx, Decision,
    DecisionKind, DecisionLog, DirEntry, FileKind, Outcome, Platform, Pressure, RetentionPolicy,
    expand_tilde,
};
use serde::Deserialize;

/// The adapter's stable machine name (config section, KV bucket, log target).
const NAME: &str = "app-temp";

/// One day, in seconds.
const DAY: u64 = 24 * 60 * 60;

/// The staging directory inside an app's temp folder.
const TEMPORARY_ITEMS: &str = "TemporaryItems";

/// How many of a candidate's largest units its label names.
const NAMED_UNITS: usize = 3;

/// How many failed units an error message names before summarising the rest.
const NAMED_FAILURES: usize = 3;

// ── configuration ────────────────────────────────────────────────────────────

/// The `[adapters.app-temp]` config table. Unknown keys are rejected so a typo
/// such as `exclude_bundle` fails loudly instead of silently excluding nothing.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    /// Normal mode: propose units with no activity for at least this long.
    #[serde(with = "humantime_serde")]
    max_age: Duration,
    /// Scavenge floor: never propose a unit with activity more recent than this.
    #[serde(with = "humantime_serde")]
    min_age: Duration,
    /// Queue candidates for `disk-saver review` instead of deleting them
    /// unattended. On by default: this is [`Class::UserData`].
    confirm: bool,
    /// Container folder names (bundle ids) never to look inside.
    exclude_bundles: Vec<String>,
    /// Where the app containers live; a leading `~` is expanded at run time.
    containers_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_age: Duration::from_secs(30 * DAY),
            min_age: Duration::from_secs(7 * DAY),
            confirm: true,
            exclude_bundles: Vec::new(),
            containers_dir: PathBuf::from("~/Library/Containers"),
        }
    }
}

/// Build the `app-temp` adapter from its `[adapters.app-temp]` table.
pub fn factory() -> AdapterFactory {
    AdapterFactory::typed(NAME, build)
}

/// Validate the config: thresholds in order, a containers directory that cannot
/// be resolved against the working directory, and bundle ids rather than paths.
fn build(cfg: Config, cx: &ConfigCx) -> Result<AppTemp, ConfigError> {
    let dir = &cfg.containers_dir;
    if !(dir.is_absolute() || dir.starts_with("~")) {
        return Err(cx.err(format!(
            "containers_dir must be absolute or start with `~`, got {}",
            dir.display()
        )));
    }
    if let Some(bad) = cfg
        .exclude_bundles
        .iter()
        .find(|b| b.is_empty() || b.contains('/'))
    {
        return Err(cx.err(format!(
            "exclude_bundles takes container folder names such as \"com.apple.MobileSMS\", \
             got {bad:?}"
        )));
    }
    Ok(AppTemp {
        policy: cx.retention(cfg.max_age, cfg.min_age)?,
        confirm: cfg.confirm,
        exclude: cfg.exclude_bundles.into_iter().collect(),
        containers_dir: cfg.containers_dir,
    })
}

// ── the adapter ──────────────────────────────────────────────────────────────

/// The app-temp adapter. Built by [`factory`].
struct AppTemp {
    policy: RetentionPolicy,
    confirm: bool,
    exclude: BTreeSet<String>,
    /// As configured, possibly with a leading `~`.
    containers_dir: PathBuf,
}

/// A unit old enough to propose (or, in `execute`, to remove).
struct Item {
    path: PathBuf,
    /// Newest activity anywhere inside.
    activity: SystemTime,
    /// Apparent size.
    bytes: u64,
}

impl Adapter for AppTemp {
    fn name(&self) -> &'static str {
        NAME
    }

    /// For `disk-saver doctor`: can the containers directory be listed, and can
    /// at least one temp folder in it be read? Only lists directories; never
    /// walks a unit.
    fn check(&mut self, ctx: &mut Ctx) -> Result<(), AdapterError> {
        let p = ctx.platform;
        let root = self.root(p);
        let Some(containers) = list_containers(p, &root)? else {
            return Ok(());
        };
        let mut access = Access::default();
        for container in &containers {
            match open_tmp(p, container) {
                Opened::Ready { .. } => match p.read_dir(&tmp_of(&container.path)) {
                    Ok(_) => access.readable += 1,
                    Err(e) if e.kind() == io::ErrorKind::PermissionDenied => access.denied += 1,
                    Err(_) => {}
                },
                Opened::Denied(_) => access.denied += 1,
                Opened::Absent | Opened::Skip(_) => {}
            }
        }
        access.verdict(&root)
    }

    /// Nothing to record: age comes from the filesystem, and looking inside
    /// other apps' containers is deferred to `plan` so that comfortable runs
    /// make no access attempts at all.
    fn observe(&mut self, _ctx: &mut Ctx) -> Result<(), AdapterError> {
        Ok(())
    }

    fn plan(&mut self, ctx: &mut Ctx) -> Result<Vec<Candidate>, AdapterError> {
        let Some(threshold) = self.threshold(ctx.pressure) else {
            return Ok(Vec::new());
        };
        let p = ctx.platform;
        let root = self.root(p);
        let Some(containers) = list_containers(p, &root)? else {
            return Ok(Vec::new());
        };
        let now = ctx.now();
        let mut access = Access::default();
        let mut candidates = Vec::new();

        for container in containers {
            let bundle = container.file_name.as_str();
            let tmp = tmp_of(&container.path);
            let id = tmp.to_string_lossy().into_owned();

            if self.exclude.contains(bundle) {
                kept(ctx, &id, bundle, "excluded by exclude_bundles");
                continue;
            }
            let dev = match open_tmp(p, &container) {
                Opened::Absent => continue,
                Opened::Skip(reason) => {
                    kept(ctx, &id, bundle, &reason);
                    continue;
                }
                Opened::Denied(error) => {
                    access.denied += 1;
                    kept(ctx, &id, bundle, &denied(&error));
                    continue;
                }
                Opened::Ready { dev } => dev,
            };
            ctx.claim(tmp.clone());

            let found = match collect_units(p, &tmp, bundle) {
                Ok(found) => found,
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                    access.denied += 1;
                    kept(ctx, &id, bundle, &denied(&e.to_string()));
                    continue;
                }
                Err(e) => {
                    kept(ctx, &id, bundle, &format!("cannot list temp folder: {e}"));
                    continue;
                }
            };
            access.readable += 1;

            let mut tally = found.passed_over;
            let mut old = Vec::new();
            for path in found.units {
                let last = match assess(p, &path, dev) {
                    Ok(last) => last,
                    Err(why) => {
                        tally.add(why);
                        continue;
                    }
                };
                let age = now.duration_since(last).unwrap_or_default();
                if !self.policy.eligible(age, ctx.pressure) {
                    tally.add(Why::Recent);
                    continue;
                }
                match p.dir_size(&path) {
                    Ok(bytes) => old.push(Item {
                        path,
                        activity: last,
                        bytes,
                    }),
                    Err(_) => tally.add(Why::Unsized),
                }
            }

            if let Some(summary) = tally.summary(threshold) {
                kept(ctx, &id, bundle, &summary);
            }
            let Some(last_used) = old.iter().map(|i| i.activity).max() else {
                continue;
            };
            let bytes = total_bytes(&old);
            if bytes == 0 {
                // Empty directories and zero-byte files: not worth a review prompt.
                kept(
                    ctx,
                    &id,
                    bundle,
                    &format!(
                        "nothing to reclaim from {} old item{}",
                        old.len(),
                        plural(old.len())
                    ),
                );
                continue;
            }
            candidates.push(
                Candidate::new(
                    id,
                    label(p, bundle, &old, now),
                    bytes,
                    last_used,
                    Class::UserData,
                )
                .confirm(self.confirm),
            );
        }

        access.verdict(&root)?;
        record_plan(ctx, NAME, &candidates);
        Ok(candidates)
    }

    fn execute(
        &mut self,
        ctx: &mut Ctx,
        batch: &[Candidate],
    ) -> Result<Vec<Outcome>, AdapterError> {
        let root = self.root(ctx.platform);
        let outcomes: Vec<Outcome> = batch
            .iter()
            .map(|c| self.execute_one(ctx, &root, c))
            .collect();
        record_results(ctx, NAME, &outcomes);
        Ok(outcomes)
    }
}

impl AppTemp {
    /// The containers directory with `~` expanded.
    fn root(&self, p: &dyn Platform) -> PathBuf {
        expand_tilde(&self.containers_dir, &p.home_dir())
    }

    /// The age a unit must reach at `pressure`, or `None` when nothing may be
    /// deleted at all. Used for early return and for kept-decision wording;
    /// eligibility itself always goes through [`RetentionPolicy::eligible`].
    fn threshold(&self, pressure: Pressure) -> Option<Duration> {
        match pressure {
            Pressure::Comfortable => None,
            Pressure::Normal => Some(self.policy.max_age),
            Pressure::Scavenge { .. } => Some(self.policy.min_age),
        }
    }

    /// Clean one approved temp folder. See the crate docs for the rules and
    /// the choice of outcome.
    fn execute_one(&self, ctx: &Ctx, root: &Path, c: &Candidate) -> Outcome {
        let p = ctx.platform;
        let skip = |reason: String| Outcome::Skipped {
            id: c.id.clone(),
            reason,
        };

        // The id must name `<root>/<bundle>/Data/tmp` exactly — never anything
        // else, however it got into the queue.
        let tmp = PathBuf::from(&c.id);
        let Some(bundle) = bundle_of(root, &tmp) else {
            return skip(format!("not an app temp folder under {}", root.display()));
        };
        // Checked again here so that excluding a bundle also stops an approval
        // that was already sitting in the queue.
        if self.exclude.contains(bundle) {
            return skip("excluded by exclude_bundles".to_owned());
        }
        let dev = match p.metadata(&tmp) {
            Ok(m) if m.kind == FileKind::Dir && real_path(p, &tmp) => m.dev,
            _ => return skip("temp folder is gone or is no longer a real directory".to_owned()),
        };
        let found = match collect_units(p, &tmp, bundle) {
            Ok(found) => found,
            Err(e) => return skip(format!("cannot list temp folder: {e}")),
        };

        // The same rules as planning, with the approval as the bar: nothing
        // newer than the newest unit the reviewer was shown.
        let doomed: Vec<Item> = found
            .units
            .into_iter()
            .filter_map(|path| {
                let last = assess(p, &path, dev).ok()?;
                if last > c.last_used {
                    return None;
                }
                let bytes = p.dir_size(&path).ok()?;
                Some(Item {
                    path,
                    activity: last,
                    bytes,
                })
            })
            .collect();
        if doomed.is_empty() {
            return skip("nothing left that is as old as what was approved".to_owned());
        }
        let total = total_bytes(&doomed);
        if total > c.bytes {
            return skip(format!(
                "found {} of old items where {} was approved; the folder gained old items \
                 since, so it will be proposed again",
                human_bytes(total),
                human_bytes(c.bytes)
            ));
        }

        let now = ctx.now();
        let mut removed = 0usize;
        let mut freed = 0u64;
        let mut failures = Vec::new();
        for item in &doomed {
            // Named before removal: afterwards there is nothing left to look at.
            let name = if DecisionLog::compiled() {
                format!("{} in {bundle} temp", unit_name(p, &item.path))
            } else {
                String::new()
            };
            let decision = |kind| {
                Decision::new(kind, NAME, item.path.to_string_lossy())
                    .label(name.clone())
                    .bytes(item.bytes)
                    .age(now.duration_since(item.activity).unwrap_or_default())
            };
            match remove(p, &item.path) {
                Ok(true) => {
                    removed += 1;
                    freed = freed.saturating_add(item.bytes);
                    if DecisionLog::compiled() {
                        ctx.decide(decision(DecisionKind::Deleted));
                    }
                }
                Ok(false) => {}
                Err(e) => {
                    if DecisionLog::compiled() {
                        ctx.decide(decision(DecisionKind::Failed).reason(e.to_string()));
                    }
                    failures.push(format!("{}: {e}", relative(&tmp, &item.path)));
                }
            }
        }

        if !failures.is_empty() {
            return Outcome::Failed {
                id: c.id.clone(),
                error: format!(
                    "removed {removed} of {} old items ({}); could not remove {}",
                    doomed.len(),
                    human_bytes(freed),
                    first_few(&failures, NAMED_FAILURES)
                ),
            };
        }
        if removed == 0 {
            return skip("everything approved had already gone".to_owned());
        }
        Outcome::Removed {
            id: c.id.clone(),
            bytes: freed,
        }
    }
}

// ── containers and temp folders ──────────────────────────────────────────────

/// The containers directory's entries, sorted by name; `None` when there is no
/// such directory, which is every Linux machine.
fn list_containers(p: &dyn Platform, root: &Path) -> Result<Option<Vec<DirEntry>>, AdapterError> {
    match p.metadata(root) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(unreadable(root, &e)),
    }
    if !real_path(p, root) {
        return Err(AdapterError::unavailable(format!(
            "{} is not a real directory: a symlink anywhere in its path is never followed",
            root.display()
        )));
    }
    match p.read_dir(root) {
        Ok(mut entries) => {
            entries.sort_by(|a, b| a.file_name.cmp(&b.file_name));
            Ok(Some(entries))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(unreadable(root, &e)),
    }
}

/// Map a failure to read the containers directory to
/// [`AdapterError::Unavailable`], with the Full Disk Access hint when macOS
/// refused it.
fn unreadable(root: &Path, e: &io::Error) -> AdapterError {
    if e.kind() == io::ErrorKind::PermissionDenied {
        AdapterError::unavailable(format!(
            "cannot read app containers {}: {e} — on macOS, grant Full Disk Access to \
             disk-saver (see `disk-saver doctor`)",
            root.display()
        ))
    } else {
        AdapterError::unavailable(format!(
            "cannot read app containers {}: {e}",
            root.display()
        ))
    }
}

/// How many temp folders could be read and how many macOS refused.
#[derive(Default)]
struct Access {
    readable: usize,
    denied: usize,
}

impl Access {
    /// [`AdapterError::Unavailable`] when every temp folder that exists was
    /// refused. That is what a launchd job without Full Disk Access sees, and
    /// without this it would plan nothing, every run, with `status` showing
    /// nothing wrong.
    fn verdict(&self, root: &Path) -> Result<(), AdapterError> {
        if self.readable == 0 && self.denied > 0 {
            return Err(AdapterError::unavailable(format!(
                "cannot read any app temp folder under {} ({} refused) — on macOS, grant \
                 Full Disk Access to disk-saver (see `disk-saver doctor`)",
                root.display(),
                self.denied
            )));
        }
        Ok(())
    }
}

/// The reason recorded for a container macOS would not let us read.
fn denied(error: &str) -> String {
    tracing::debug!(adapter = NAME, %error, "app temp folder refused");
    format!("cannot read: {error} — grant Full Disk Access to disk-saver to include it")
}

/// `<container>/Data/tmp`.
fn tmp_of(container: &Path) -> PathBuf {
    container.join("Data").join("tmp")
}

/// The bundle id in `<root>/<bundle>/Data/tmp`, or `None` for any other shape.
fn bundle_of<'a>(root: &Path, tmp: &'a Path) -> Option<&'a str> {
    let mut parts = tmp.strip_prefix(root).ok()?.components();
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (
            Some(Component::Normal(bundle)),
            Some(Component::Normal(data)),
            Some(Component::Normal(tmp)),
            None,
        ) if data == "Data" && tmp == "tmp" => bundle.to_str(),
        _ => None,
    }
}

/// What a container's `Data/tmp` turned out to be.
enum Opened {
    /// No temp folder here: nothing to do, nothing worth recording.
    Absent,
    /// A temp folder this adapter will not walk, and why.
    Skip(String),
    /// macOS refused access.
    Denied(String),
    /// A real directory on device `dev`.
    Ready { dev: u64 },
}

/// Check that `container`, its `Data` and its `tmp` are real directories.
fn open_tmp(p: &dyn Platform, container: &DirEntry) -> Opened {
    match container.kind {
        FileKind::Dir => {}
        FileKind::Symlink => {
            return Opened::Skip("container is a symlink; not followed".to_owned());
        }
        // A stray file such as `.DS_Store`.
        FileKind::File | FileKind::Other => return Opened::Absent,
    }
    let data = container.path.join("Data");
    let tmp = data.join("tmp");
    let mut dev = 0;
    for (dir, name) in [(&data, "Data"), (&tmp, "Data/tmp")] {
        match p.metadata(dir) {
            Ok(m) if m.kind == FileKind::Dir => dev = m.dev,
            Ok(m) if m.kind == FileKind::Symlink => {
                return Opened::Skip(format!("{name} is a symlink; not followed"));
            }
            Ok(_) => return Opened::Absent,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Opened::Absent,
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                return Opened::Denied(e.to_string());
            }
            Err(e) => return Opened::Skip(format!("cannot inspect {name}: {e}")),
        }
    }
    Opened::Ready { dev }
}

// ── units ────────────────────────────────────────────────────────────────────

/// Where a directory sits, for the few directories in a temp folder that hold
/// independent items rather than being one.
#[derive(Clone, Copy)]
enum Staging {
    /// `tmp` itself.
    Tmp,
    /// `tmp/TemporaryItems`.
    TemporaryItems,
    /// `tmp/TemporaryItems/<bundle-id>`, the app's own staging root.
    AppRoot,
    /// A directory in the app's staging root, such as Messages' `Media`.
    AppShelf,
}

impl Staging {
    /// The staging level that a child directory called `name` opens, or `None`
    /// when that child is a unit.
    fn inner(self, name: &str, bundle: &str) -> Option<Staging> {
        match self {
            Staging::Tmp => (name == TEMPORARY_ITEMS).then_some(Staging::TemporaryItems),
            Staging::TemporaryItems => (name == bundle).then_some(Staging::AppRoot),
            Staging::AppRoot => Some(Staging::AppShelf),
            Staging::AppShelf => None,
        }
    }
}

/// A temp folder's units, and a tally of what was passed over finding them.
struct Found {
    units: Vec<PathBuf>,
    passed_over: Tally,
}

/// Collect the units of the temp folder `tmp` (see the crate docs). An error
/// listing `tmp` itself is returned; an unreadable staging directory inside it
/// is tallied and skipped.
fn collect_units(p: &dyn Platform, tmp: &Path, bundle: &str) -> io::Result<Found> {
    let mut found = Found {
        units: Vec::new(),
        passed_over: Tally::default(),
    };
    collect(p, tmp, Staging::Tmp, bundle, &mut found)?;
    Ok(found)
}

/// Walk one staging directory. Depth is bounded by [`Staging`]: at most four
/// levels below `tmp`.
fn collect(
    p: &dyn Platform,
    dir: &Path,
    level: Staging,
    bundle: &str,
    found: &mut Found,
) -> io::Result<()> {
    let mut entries = p.read_dir(dir)?;
    entries.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    for entry in entries {
        match entry.kind {
            FileKind::Dir => match level.inner(&entry.file_name, bundle) {
                Some(inner) => {
                    if collect(p, &entry.path, inner, bundle, found).is_err() {
                        found.passed_over.add(Why::UnreadableFolder);
                    }
                }
                None => found.units.push(entry.path),
            },
            FileKind::File => found.units.push(entry.path),
            FileKind::Symlink => found.passed_over.add(Why::Symlink),
            // Sockets and fifos in a temp folder are live IPC endpoints.
            FileKind::Other => found.passed_over.add(Why::Special),
        }
    }
    Ok(())
}

/// A unit's activity, or why it cannot be judged. Re-reads the unit's own
/// metadata rather than trusting the listing, and refuses anything on another
/// filesystem than its temp folder.
fn assess(p: &dyn Platform, unit: &Path, dev: u64) -> Result<SystemTime, Why> {
    let meta = p.metadata(unit).map_err(|_| Why::Unknown)?;
    match meta.kind {
        FileKind::File | FileKind::Dir => {}
        FileKind::Symlink => return Err(Why::Symlink),
        FileKind::Other => return Err(Why::Special),
    }
    if meta.dev != dev {
        return Err(Why::OtherFilesystem);
    }
    activity(p, unit).ok_or(Why::Unknown)
}

/// Remove one unit: `Ok(true)` when removed, `Ok(false)` when it had already
/// gone or is no longer a plain file or directory (left alone).
fn remove(p: &dyn Platform, unit: &Path) -> io::Result<bool> {
    let result = match p.metadata(unit) {
        Ok(m) if m.kind == FileKind::Dir => p.remove_dir_all(unit),
        Ok(m) if m.kind == FileKind::File => p.remove_file(unit),
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    match result {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

// ── kept decisions ───────────────────────────────────────────────────────────

/// Why a unit was not proposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Why {
    /// Active within the threshold.
    Recent,
    /// Its activity could not be established.
    Unknown,
    /// Old, but could not be sized.
    Unsized,
    /// A staging directory that could not be listed.
    UnreadableFolder,
    /// A symlink.
    Symlink,
    /// A socket, fifo or device.
    Special,
    /// On another filesystem than its temp folder.
    OtherFilesystem,
}

impl Why {
    fn describe(self, n: usize, threshold: &str) -> String {
        let s = plural(n);
        match self {
            Why::Recent => format!("{n} active within {threshold}"),
            Why::Unknown => format!("{n} whose activity could not be established"),
            Why::Unsized => format!("{n} that could not be sized"),
            Why::UnreadableFolder => format!("{n} unreadable staging folder{s}"),
            Why::Symlink => format!("{n} symlink{s} (never followed)"),
            Why::Special => format!("{n} special file{s}"),
            Why::OtherFilesystem => format!("{n} on another filesystem"),
        }
    }
}

/// Counts of [`Why`], summarised into one kept decision per container rather
/// than one per unit: `com.apple.geod` alone holds a thousand.
#[derive(Default)]
struct Tally(BTreeMap<Why, usize>);

impl Tally {
    fn add(&mut self, why: Why) {
        *self.0.entry(why).or_default() += 1;
    }

    fn summary(&self, threshold: Duration) -> Option<String> {
        if self.0.is_empty() {
            return None;
        }
        let total: usize = self.0.values().sum();
        let threshold = human_age(threshold);
        let parts: Vec<String> = self
            .0
            .iter()
            .map(|(why, &n)| why.describe(n, &threshold))
            .collect();
        Some(format!(
            "kept {total} item{}: {}",
            plural(total),
            parts.join(", ")
        ))
    }
}

/// Record a kept decision about a container's temp folder.
fn kept(ctx: &Ctx, id: &str, bundle: &str, reason: &str) {
    if DecisionLog::compiled() {
        ctx.decide(
            Decision::new(DecisionKind::Kept, NAME, id)
                .label(bundle)
                .reason(reason),
        );
    }
}

// ── labels ───────────────────────────────────────────────────────────────────

/// `4 old items in com.apple.MobileSMS temp (oldest 8 months, newest 31 days):
/// Some Installer-1.0.pkg 8.6 GB, …`, naming the largest units.
fn label(p: &dyn Platform, bundle: &str, old: &[Item], now: SystemTime) -> String {
    let age = |t: SystemTime| human_age(now.duration_since(t).unwrap_or_default());
    let newest = old.iter().map(|i| i.activity).max().unwrap_or(now);
    let oldest = old.iter().map(|i| i.activity).min().unwrap_or(now);
    let ages = if old.len() == 1 {
        format!("{} old", age(newest))
    } else {
        format!("oldest {}, newest {}", age(oldest), age(newest))
    };

    let mut largest: Vec<&Item> = old.iter().collect();
    largest.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    let mut named: Vec<String> = largest
        .iter()
        .take(NAMED_UNITS)
        .map(|i| format!("{} {}", unit_name(p, &i.path), human_bytes(i.bytes)))
        .collect();
    if old.len() > NAMED_UNITS {
        named.push(format!("+{} more", old.len() - NAMED_UNITS));
    }
    format!(
        "{} old item{} in {bundle} temp ({ages}): {}",
        old.len(),
        plural(old.len()),
        named.join(", ")
    )
}

/// What to call a unit for a person. Usually its own name, but a directory that
/// only wraps one entry — Messages' `Media/<UUID>/photo.heic` — is named after
/// what it wraps, since a UUID means nothing to whoever is approving it.
fn unit_name(p: &dyn Platform, unit: &Path) -> String {
    let mut path = unit.to_path_buf();
    for _ in 0..4 {
        match p.read_dir(&path).as_deref() {
            Ok([only]) if only.kind != FileKind::Symlink => path = only.path.clone(),
            _ => break,
        }
    }
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `path` relative to `tmp`, for messages.
fn relative(tmp: &Path, path: &Path) -> String {
    path.strip_prefix(tmp).unwrap_or(path).display().to_string()
}

/// The first `n` entries joined, then `+k more`.
fn first_few(items: &[String], n: usize) -> String {
    let mut shown: Vec<String> = items.iter().take(n).cloned().collect();
    if items.len() > n {
        shown.push(format!("+{} more", items.len() - n));
    }
    shown.join(", ")
}

/// Sum of the items' sizes, saturating.
fn total_bytes(items: &[Item]) -> u64 {
    items.iter().fold(0, |sum, i| sum.saturating_add(i.bytes))
}

/// A byte count as people read it, e.g. `8.6 GB` (SI, matching the CLI).
fn human_bytes(n: u64) -> String {
    bytesize::ByteSize(n).display().si().to_string()
}

/// A coarse age as people say it: `45 minutes`, `3 hours`, `31 days`,
/// `8 months`, `2 years`. Days run to 60 so that "31 days" stays exact near
/// the default threshold; a month is counted as 30 days.
fn human_age(age: Duration) -> String {
    const HOUR: u64 = 60 * 60;
    let secs = age.as_secs();
    let (n, unit) = if secs < HOUR {
        (secs / 60, "minute")
    } else if secs < DAY {
        (secs / HOUR, "hour")
    } else if secs < 60 * DAY {
        (secs / DAY, "day")
    } else if secs < 730 * DAY {
        (secs / (30 * DAY), "month")
    } else {
        (secs / (365 * DAY), "year")
    };
    format!(
        "{n} {unit}{}",
        plural(usize::try_from(n).unwrap_or(usize::MAX))
    )
}

/// `"s"` unless `n` is one.
fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests;
