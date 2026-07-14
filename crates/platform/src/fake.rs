//! In-memory [`Platform`] for tests, behind the `test-util` feature.
//!
//! `FakePlatform` provides a fluent builder to construct a fake world (files,
//! dirs, sized dirs, symlinks, canned commands, a clock, free space), recorders
//! that capture every destructive call and every notification, and mutators to
//! advance the clock or change free space mid-test. It is `Send + Sync` via an
//! internal [`Mutex`].
//!
//! # Path handling
//!
//! A leading `~` in **any** path — builder input or query — is expanded against
//! the fake home directory. All stored keys are absolute and normalized.
//!
//! # No symlink follow
//!
//! [`Platform::metadata`], [`Platform::read_dir`], [`Platform::dir_size`] and
//! the removal methods never traverse into a symlink's target, exactly like
//! [`RealPlatform`](crate::RealPlatform).

use std::collections::BTreeMap;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{
    CommandOutput, CommandSpec, DirEntry, DiskUsage, FileKind, FileMeta, Notification, Platform,
};

/// A node in the fake filesystem tree.
#[derive(Debug, Clone)]
struct FakeNode {
    kind: NodeKind,
    modified: SystemTime,
}

/// The variants a fake node can take.
#[derive(Debug, Clone)]
enum NodeKind {
    /// A regular file with in-memory contents; its length is `contents.len()`.
    File { contents: Vec<u8> },
    /// A directory whose children are other keys in the map.
    Dir,
    /// A directory that reports a fixed [`Platform::dir_size`] without holding
    /// any materialised children (a hidden sentinel).
    SizedDir { size: u64 },
    /// A symbolic link. The target is stored but never followed.
    Symlink { target: PathBuf },
}

/// A scripted subprocess response.
#[derive(Debug, Clone)]
enum CommandRule {
    /// Matches when program and the full argument vector are equal.
    Exact {
        program: String,
        args: Vec<String>,
        output: CommandOutput,
    },
    /// Matches when program is equal and the arguments start with the prefix.
    Prefix {
        program: String,
        arg_prefix: Vec<String>,
        output: CommandOutput,
    },
    /// Matches any invocation of the program and yields an error of this kind.
    Error {
        program: String,
        kind: io::ErrorKind,
    },
}

/// Interior mutable state shared behind the [`FakePlatform`] mutex.
#[derive(Debug)]
struct FakeState {
    home: PathBuf,
    /// Explicit cache-dir override; when `None`, derived from `home`.
    cache_dir: Option<PathBuf>,
    /// Explicit trash-dir override; when `None`, derived from `home`.
    trash_dirs: Option<Vec<PathBuf>>,
    now: SystemTime,
    available: u64,
    total: u64,
    nodes: BTreeMap<PathBuf, FakeNode>,
    /// Simulated mount points: `(mount_root, device_id)`. A node's device is the
    /// id of the longest mount root that is a prefix of its path, else `1` (the
    /// default "root" device). Lets tests model a filesystem boundary nested
    /// under a scanned root (ARCHITECTURE.md §11.8).
    mounts: Vec<(PathBuf, u64)>,
    commands: Vec<CommandRule>,
    removed: Vec<PathBuf>,
    notifications: Vec<Notification>,
    commands_run: Vec<CommandSpec>,
}

/// Expand a leading `~` component against `home`, then normalize the path
/// (dropping `.` and redundant separators via [`Path::components`]).
fn normalize(path: &Path, home: &Path) -> PathBuf {
    let mut comps = path.components();
    let expanded: PathBuf = match comps.next() {
        Some(Component::Normal(first)) if first == "~" => {
            let rest: PathBuf = comps.collect();
            home.join(rest)
        }
        _ => path.components().collect(),
    };
    // Collapse `.`/redundant separators one more time for the joined form.
    expanded.components().collect()
}

impl FakeState {
    /// Effective cache directory (override, else `~/.cache`).
    fn effective_cache(&self) -> PathBuf {
        self.cache_dir
            .clone()
            .unwrap_or_else(|| self.home.join(".cache"))
    }

    /// Effective trash directories (override, else `~/.local/share/Trash`).
    fn effective_trash(&self) -> Vec<PathBuf> {
        self.trash_dirs
            .clone()
            .unwrap_or_else(|| vec![self.home.join(".local").join("share").join("Trash")])
    }

    /// The device id of `path`: the id of the longest registered mount root that
    /// is a prefix of (or equal to) `path`, else `1` (the default device).
    fn dev_of(&self, path: &Path) -> u64 {
        self.mounts
            .iter()
            .filter(|(root, _)| path == root || path.starts_with(root))
            .max_by_key(|(root, _)| root.components().count())
            .map(|(_, dev)| *dev)
            .unwrap_or(1)
    }

    /// Ensure every ancestor directory of `path` exists as a `Dir` node.
    fn ensure_ancestors(&mut self, path: &Path, modified: SystemTime) {
        let mut ancestors: Vec<PathBuf> = path
            .ancestors()
            .skip(1)
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .collect();
        // Insert from the root downward so parents precede children.
        ancestors.reverse();
        for anc in ancestors {
            self.nodes.entry(anc).or_insert(FakeNode {
                kind: NodeKind::Dir,
                modified,
            });
        }
    }

    /// Insert (or overwrite) a node, creating ancestor directories first.
    fn insert(&mut self, path: PathBuf, node: FakeNode) {
        self.ensure_ancestors(&path, node.modified);
        self.nodes.insert(path, node);
    }

    /// Direct children of `dir` (keys whose parent equals `dir`).
    fn children(&self, dir: &Path) -> Vec<(PathBuf, FakeNode)> {
        let mut out: Vec<(PathBuf, FakeNode)> = self
            .nodes
            .iter()
            .filter(|(k, _)| k.parent() == Some(dir))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Recursively sum apparent file sizes under `dir` (assumed to be a real
    /// directory). Sized-dir sentinels contribute their fixed size and are not
    /// descended; symlinks contribute nothing.
    fn dir_size_of(&self, dir: &Path) -> u64 {
        let mut total: u64 = 0;
        for (path, node) in self.children(dir) {
            match &node.kind {
                NodeKind::File { contents } => {
                    total = total.saturating_add(contents.len() as u64);
                }
                NodeKind::Dir => {
                    total = total.saturating_add(self.dir_size_of(&path));
                }
                NodeKind::SizedDir { size } => {
                    total = total.saturating_add(*size);
                }
                NodeKind::Symlink { .. } => {
                    // Never follow.
                }
            }
        }
        total
    }

    /// Resolve a command spec against the scripted rules (exact, then prefix,
    /// then error, then a `NotFound` fallback).
    fn resolve_command(&self, spec: &CommandSpec) -> io::Result<CommandOutput> {
        for rule in &self.commands {
            if let CommandRule::Exact {
                program,
                args,
                output,
            } = rule
                && *program == spec.program
                && *args == spec.args
            {
                return Ok(output.clone());
            }
        }
        for rule in &self.commands {
            if let CommandRule::Prefix {
                program,
                arg_prefix,
                output,
            } = rule
                && *program == spec.program
                && spec.args.len() >= arg_prefix.len()
                && spec.args[..arg_prefix.len()] == arg_prefix[..]
            {
                return Ok(output.clone());
            }
        }
        for rule in &self.commands {
            if let CommandRule::Error { program, kind } = rule
                && *program == spec.program
            {
                return Err(io::Error::new(
                    *kind,
                    format!("fake: scripted error for command '{program}'"),
                ));
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("fake: no canned response for command '{}'", spec.program),
        ))
    }
}

/// Map a [`NodeKind`] to the [`FileKind`] reported to callers.
fn node_file_kind(kind: &NodeKind) -> FileKind {
    match kind {
        NodeKind::File { .. } => FileKind::File,
        NodeKind::Dir | NodeKind::SizedDir { .. } => FileKind::Dir,
        NodeKind::Symlink { .. } => FileKind::Symlink,
    }
}

/// Apparent length reported by [`Platform::metadata`] for a node.
fn node_len(kind: &NodeKind) -> u64 {
    match kind {
        NodeKind::File { contents } => contents.len() as u64,
        NodeKind::Symlink { target } => target.as_os_str().len() as u64,
        NodeKind::Dir | NodeKind::SizedDir { .. } => 0,
    }
}

/// In-memory [`Platform`] for deterministic, OS-free tests.
///
/// See the [module docs](self) for the builder/recorder API and path handling.
#[derive(Debug)]
pub struct FakePlatform {
    state: Mutex<FakeState>,
}

impl FakePlatform {
    /// A fresh fake world: home `/home/tester`, cache `~/.cache`, trash
    /// `[~/.local/share/Trash]`, a fixed clock at `UNIX_EPOCH + 400 days`, and
    /// generous free/total space.
    pub fn new() -> Self {
        let now = UNIX_EPOCH + Duration::from_secs(400 * 24 * 60 * 60);
        FakePlatform {
            state: Mutex::new(FakeState {
                home: PathBuf::from("/home/tester"),
                cache_dir: None,
                trash_dirs: None,
                now,
                available: 500_000_000_000,
                total: 1_000_000_000_000,
                nodes: BTreeMap::new(),
                mounts: Vec::new(),
                commands: Vec::new(),
                removed: Vec::new(),
                notifications: Vec::new(),
                commands_run: Vec::new(),
            }),
        }
    }

    /// Lock the state, panicking with a clear message if the mutex is poisoned.
    fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().expect("FakePlatform mutex poisoned")
    }

    // ── builders (consume + return Self) ────────────────────────────────

    /// Set the fake home directory. Set this before adding `~`-based paths.
    #[must_use]
    pub fn with_home(self, home: impl Into<PathBuf>) -> Self {
        {
            let mut st = self.lock();
            let home = home.into();
            st.home = home.components().collect();
        }
        self
    }

    /// Register `path` (and its subtree) as a separate filesystem, so that
    /// [`Platform::metadata`] reports a distinct `dev` for it — modelling a mount
    /// point nested under a scanned root. Also creates the mount-point directory.
    /// Each call uses a fresh device id.
    #[must_use]
    pub fn with_mount(self, path: impl AsRef<Path>) -> Self {
        {
            let mut st = self.lock();
            let p = normalize(path.as_ref(), &st.home);
            let dev = 2 + st.mounts.len() as u64; // 1 is the default device.
            st.mounts.push((p.clone(), dev));
            let now = st.now;
            st.insert(
                p,
                FakeNode {
                    kind: NodeKind::Dir,
                    modified: now,
                },
            );
        }
        self
    }

    /// Set the fake clock.
    #[must_use]
    pub fn with_now(self, now: SystemTime) -> Self {
        {
            self.lock().now = now;
        }
        self
    }

    /// Set the reported free/total disk space (bytes).
    #[must_use]
    pub fn with_free_space(self, available: u64, total: u64) -> Self {
        {
            let mut st = self.lock();
            st.available = available;
            st.total = total;
        }
        self
    }

    /// Override the trash directories (`~` in each is expanded).
    #[must_use]
    pub fn with_trash_dirs(self, dirs: Vec<PathBuf>) -> Self {
        {
            let mut st = self.lock();
            let home = st.home.clone();
            let dirs = dirs.iter().map(|d| normalize(d, &home)).collect();
            st.trash_dirs = Some(dirs);
        }
        self
    }

    /// Override the user cache directory (`~` is expanded).
    #[must_use]
    pub fn with_cache_dir(self, dir: impl Into<PathBuf>) -> Self {
        {
            let mut st = self.lock();
            let home = st.home.clone();
            let dir = normalize(&dir.into(), &home);
            st.cache_dir = Some(dir);
        }
        self
    }

    /// Create a file (auto-creating ancestor directories). The file's length is
    /// the length of `contents`.
    #[must_use]
    pub fn with_file(
        self,
        path: impl AsRef<Path>,
        contents: impl Into<Vec<u8>>,
        modified: SystemTime,
    ) -> Self {
        {
            let mut st = self.lock();
            let home = st.home.clone();
            let p = normalize(path.as_ref(), &home);
            st.insert(
                p,
                FakeNode {
                    kind: NodeKind::File {
                        contents: contents.into(),
                    },
                    modified,
                },
            );
        }
        self
    }

    /// Create an empty directory (auto-creating ancestors).
    #[must_use]
    pub fn with_dir(self, path: impl AsRef<Path>, modified: SystemTime) -> Self {
        {
            let mut st = self.lock();
            let home = st.home.clone();
            let p = normalize(path.as_ref(), &home);
            st.insert(
                p,
                FakeNode {
                    kind: NodeKind::Dir,
                    modified,
                },
            );
        }
        self
    }

    /// Create a directory whose [`Platform::dir_size`] is exactly `size`,
    /// without materialising any files. [`Platform::read_dir`] on it is empty.
    #[must_use]
    pub fn with_sized_dir(self, path: impl AsRef<Path>, size: u64, modified: SystemTime) -> Self {
        {
            let mut st = self.lock();
            let home = st.home.clone();
            let p = normalize(path.as_ref(), &home);
            st.insert(
                p,
                FakeNode {
                    kind: NodeKind::SizedDir { size },
                    modified,
                },
            );
        }
        self
    }

    /// Create a symbolic link at `path` pointing at `target` (never followed).
    #[must_use]
    pub fn with_symlink(
        self,
        path: impl AsRef<Path>,
        target: impl Into<PathBuf>,
        modified: SystemTime,
    ) -> Self {
        {
            let mut st = self.lock();
            let home = st.home.clone();
            let p = normalize(path.as_ref(), &home);
            let target = normalize(&target.into(), &home);
            st.insert(
                p,
                FakeNode {
                    kind: NodeKind::Symlink { target },
                    modified,
                },
            );
        }
        self
    }

    /// Register a canned command matched on program + the full argument list.
    #[must_use]
    pub fn with_command(self, program: &str, args: &[&str], output: CommandOutput) -> Self {
        {
            self.lock().commands.push(CommandRule::Exact {
                program: program.to_string(),
                args: args.iter().map(|s| s.to_string()).collect(),
                output,
            });
        }
        self
    }

    /// Register a canned command matched on program + an argument prefix.
    #[must_use]
    pub fn with_command_prefix(
        self,
        program: &str,
        arg_prefix: &[&str],
        output: CommandOutput,
    ) -> Self {
        {
            self.lock().commands.push(CommandRule::Prefix {
                program: program.to_string(),
                arg_prefix: arg_prefix.iter().map(|s| s.to_string()).collect(),
                output,
            });
        }
        self
    }

    /// Register a scripted error for any invocation of `program` (e.g. a dead
    /// daemon or missing binary).
    #[must_use]
    pub fn with_command_error(self, program: &str, kind: io::ErrorKind) -> Self {
        {
            self.lock().commands.push(CommandRule::Error {
                program: program.to_string(),
                kind,
            });
        }
        self
    }

    // ── mutation during a test (take &self) ─────────────────────────────

    /// Set the fake clock.
    pub fn set_now(&self, now: SystemTime) {
        self.lock().now = now;
    }

    /// Advance the fake clock by `by`.
    pub fn advance(&self, by: Duration) {
        let mut st = self.lock();
        st.now += by;
    }

    /// Set the reported free/total disk space (bytes).
    pub fn set_free_space(&self, available: u64, total: u64) {
        let mut st = self.lock();
        st.available = available;
        st.total = total;
    }

    // ── recorders / assertions ──────────────────────────────────────────

    /// Paths removed via [`Platform::remove_file`] / [`Platform::remove_dir_all`],
    /// in call order.
    pub fn removed(&self) -> Vec<PathBuf> {
        self.lock().removed.clone()
    }

    /// Notifications delivered via [`Platform::notify`], in call order.
    pub fn notifications(&self) -> Vec<Notification> {
        self.lock().notifications.clone()
    }

    /// Command specs passed to [`Platform::run_command`], in call order.
    pub fn commands_run(&self) -> Vec<CommandSpec> {
        self.lock().commands_run.clone()
    }

    /// Whether a node exists at `path` (`~` expanded, no symlink follow).
    pub fn exists(&self, path: impl AsRef<Path>) -> bool {
        let st = self.lock();
        let p = normalize(path.as_ref(), &st.home);
        st.nodes.contains_key(&p)
    }
}

impl Default for FakePlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl Platform for FakePlatform {
    fn now(&self) -> SystemTime {
        self.lock().now
    }

    fn home_dir(&self) -> PathBuf {
        self.lock().home.clone()
    }

    fn trash_dirs(&self) -> Vec<PathBuf> {
        self.lock().effective_trash()
    }

    fn user_cache_dir(&self) -> PathBuf {
        self.lock().effective_cache()
    }

    fn metadata(&self, path: &Path) -> io::Result<FileMeta> {
        let st = self.lock();
        let p = normalize(path, &st.home);
        match st.nodes.get(&p) {
            Some(node) => Ok(FileMeta {
                kind: node_file_kind(&node.kind),
                len: node_len(&node.kind),
                modified: node.modified,
                dev: st.dev_of(&p),
            }),
            None => Err(not_found(&p)),
        }
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        let st = self.lock();
        let p = normalize(path, &st.home);
        match st.nodes.get(&p) {
            None => Err(not_found(&p)),
            Some(node) => match &node.kind {
                NodeKind::Dir => Ok(st
                    .children(&p)
                    .into_iter()
                    .map(|(child, node)| DirEntry {
                        file_name: child
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        path: child,
                        kind: node_file_kind(&node.kind),
                    })
                    .collect()),
                // A sized-dir sentinel has no materialised children.
                NodeKind::SizedDir { .. } => Ok(Vec::new()),
                _ => Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!("fake: not a directory: {}", p.display()),
                )),
            },
        }
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        let st = self.lock();
        let p = normalize(path, &st.home);
        match st.nodes.get(&p) {
            None => Err(not_found(&p)),
            Some(node) => match &node.kind {
                NodeKind::File { contents } => String::from_utf8(contents.clone()).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("fake: file is not valid UTF-8: {}", p.display()),
                    )
                }),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("fake: not a readable file: {}", p.display()),
                )),
            },
        }
    }

    fn dir_size(&self, path: &Path) -> io::Result<u64> {
        let st = self.lock();
        let p = normalize(path, &st.home);
        match st.nodes.get(&p) {
            None => Err(not_found(&p)),
            Some(node) => match &node.kind {
                NodeKind::File { contents } => Ok(contents.len() as u64),
                NodeKind::SizedDir { size } => Ok(*size),
                // Never follow a top-level symlink.
                NodeKind::Symlink { .. } => Ok(0),
                NodeKind::Dir => Ok(st.dir_size_of(&p)),
            },
        }
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        let mut st = self.lock();
        let p = normalize(path, &st.home);
        match st.nodes.get(&p) {
            None => Err(not_found(&p)),
            Some(node) if matches!(node.kind, NodeKind::Dir | NodeKind::SizedDir { .. }) => {
                Err(io::Error::new(
                    io::ErrorKind::IsADirectory,
                    format!("fake: is a directory: {}", p.display()),
                ))
            }
            Some(_) => {
                st.nodes.remove(&p);
                st.removed.push(p);
                Ok(())
            }
        }
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        let mut st = self.lock();
        let p = normalize(path, &st.home);
        let Some(node) = st.nodes.get(&p) else {
            return Err(not_found(&p));
        };

        if matches!(node.kind, NodeKind::Symlink { .. }) {
            // Unlink the symlink itself; never descend into the target.
            st.nodes.remove(&p);
            st.removed.push(p);
            return Ok(());
        }

        // Never cross a filesystem boundary (§11.8): any node under `p` that
        // lives on a different device is a mount and must survive, along with the
        // same-device chain of directories leading down to it (so, as on a real
        // fs, the parent cannot be removed and the whole call fails).
        let root_dev = st.dev_of(&p);
        let keep_mount: Vec<PathBuf> = st
            .nodes
            .keys()
            .filter(|k| (*k == &p || k.starts_with(&p)) && st.dev_of(k) != root_dev)
            .cloned()
            .collect();

        if keep_mount.is_empty() {
            // Simple case: remove the node and every descendant key.
            // `Path::starts_with` is component-aware, so `/a/bc` is not under `/a/b`.
            st.nodes.retain(|k, _| k != &p && !k.starts_with(&p));
            st.removed.push(p);
            return Ok(());
        }

        let to_remove: Vec<PathBuf> = st
            .nodes
            .keys()
            .filter(|k| {
                (*k == &p || k.starts_with(&p))
                    && st.dev_of(k) == root_dev
                    && !keep_mount.iter().any(|m| m.starts_with(k))
            })
            .cloned()
            .collect();
        for k in &to_remove {
            st.nodes.remove(k);
        }
        st.removed.extend(to_remove);
        Err(io::Error::new(
            io::ErrorKind::DirectoryNotEmpty,
            format!(
                "fake: refusing to remove across a filesystem boundary under {}",
                p.display()
            ),
        ))
    }

    fn disk_usage(&self, _path: &Path) -> io::Result<DiskUsage> {
        let st = self.lock();
        Ok(DiskUsage {
            total: st.total,
            available: st.available,
        })
    }

    fn notify(&self, n: &Notification) -> io::Result<()> {
        self.lock().notifications.push(n.clone());
        Ok(())
    }

    fn run_command(&self, cmd: &CommandSpec) -> io::Result<CommandOutput> {
        let mut st = self.lock();
        st.commands_run.push(cmd.clone());
        st.resolve_command(cmd)
    }
}

/// Build a `NotFound` error mentioning `path`.
fn not_found(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("fake: no such path: {}", path.display()),
    )
}
