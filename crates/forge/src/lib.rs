//! Asks a forge whether a branch's merge request or pull request merged.
//!
//! The branch's remote (`branch.<name>.remote`, else `origin`) names the
//! repository, and its host picks the forge. The question goes to the forge's
//! REST API — GitHub's pinned to API version [`GITHUB_API_VERSION`], GitLab's
//! v4 — through the forge CLI's `api` command, which adds the login the CLI
//! already has and nothing else. So what this depends on is each forge's
//! documented, versioned API, not the CLIs' own commands and output formats,
//! and disk-saver never holds a token.
//!
//! Any failure is an answer of its own ([`ForgeState::Unknown`]) that says
//! why, so "couldn't ask" is never mistaken for "not merged".

use std::collections::BTreeMap;
use std::fmt;
use std::io::ErrorKind;
use std::path::Path;
use std::time::Duration;

use disk_saver_platform::{CommandOutput, CommandSpec, Platform};
use serde::Deserialize;

/// How long one forge call may take.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The GitHub REST API version asked for. GitHub supports a version for at
/// least 24 months after its successor ships.
pub const GITHUB_API_VERSION: &str = "2022-11-28";

/// Which forge serves a host. Deserializes from `"github"`, `"gitlab"` or
/// `"none"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForgeKind {
    Github,
    Gitlab,
    /// Never ask about this host.
    None,
}

/// A merge request (GitLab) or pull request (GitHub).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    kind: ForgeKind,
    number: u64,
}

impl fmt::Display for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            ForgeKind::Github => write!(f, "PR #{}", self.number),
            ForgeKind::Gitlab | ForgeKind::None => write!(f, "MR !{}", self.number),
        }
    }
}

/// What the forge said about a branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgeState {
    /// At least one request from the branch is still open.
    Open(Request),
    /// None is open and one merged (the newest, if several did).
    Merged(Request),
    /// Every request from the branch was closed without merging.
    Closed(Request),
    /// The forge has no request from the branch.
    NoRequest(ForgeKind),
    /// No answer, and why.
    Unknown(String),
    /// HEAD is detached, so there is no branch to ask about.
    Detached,
}

impl fmt::Display for ForgeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ForgeState::Open(r) => write!(f, "{r} is open"),
            ForgeState::Merged(r) => write!(f, "{r} merged"),
            ForgeState::Closed(r) => write!(f, "{r} closed without merging"),
            ForgeState::NoRequest(ForgeKind::Github) => f.write_str("no PR from this branch"),
            ForgeState::NoRequest(_) => f.write_str("no MR from this branch"),
            ForgeState::Unknown(reason) => write!(f, "couldn't look up its MR or PR: {reason}"),
            ForgeState::Detached => f.write_str("no branch to look up"),
        }
    }
}

/// A remote URL reduced to what a forge API needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    /// Lowercase, without user or port.
    pub host: String,
    /// `owner/repo` or `group/subgroup/repo`, without `.git`.
    pub path: String,
}

impl Remote {
    /// Parse `https://host/path`, `ssh://user@host:port/path` or the scp-like
    /// `user@host:path`. `None` for a local path, a `file://` URL, or anything
    /// without both a host and an `owner/repo` path.
    pub fn parse(url: &str) -> Option<Remote> {
        let url = url.trim();
        let (authority, path) = match url.split_once("://") {
            Some((scheme, rest)) => {
                if scheme.eq_ignore_ascii_case("file") {
                    return None;
                }
                let (authority, path) = rest.split_once('/')?;
                let host = authority.rsplit('@').next()?;
                (host.split(':').next()?, path)
            }
            None => {
                let (authority, path) = url.split_once(':')?;
                // `./a:b` or `/srv/a:b` is a local path, not `host:path`.
                if authority.contains('/') {
                    return None;
                }
                (authority.rsplit('@').next()?, path)
            }
        };
        let path = path.trim_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        if authority.is_empty() || !path.contains('/') {
            return None;
        }
        Some(Remote {
            host: authority.to_ascii_lowercase(),
            path: path.to_string(),
        })
    }
}

/// The forge serving `host`: the config's entry, else one recognised by name.
pub fn kind_for(host: &str, forges: &BTreeMap<String, ForgeKind>) -> ForgeKind {
    if let Some(kind) = forges.get(host) {
        return *kind;
    }
    if host == "github.com" || host.starts_with("github.") {
        ForgeKind::Github
    } else if host == "gitlab.com" || host.starts_with("gitlab.") {
        ForgeKind::Gitlab
    } else {
        ForgeKind::None
    }
}

/// The remote the branch checked out in `dir` pushes to. `Err` says why there
/// is none to ask about. The URL itself stays out of every message: it can
/// carry a token.
pub fn remote_of(p: &dyn Platform, dir: &Path, branch: &str) -> Result<Remote, String> {
    let dir = dir.to_string_lossy();
    let git = |args: &[&str]| {
        let mut all = vec!["-C", dir.as_ref()];
        all.extend_from_slice(args);
        p.run_command(&CommandSpec::new("git", all))
    };
    let name = match git(&["config", "--get", &format!("branch.{branch}.remote")]) {
        Ok(out) if out.success() => out.stdout_string().trim().to_string(),
        // Exit 1: the key is unset, so the branch tracks nothing.
        Ok(out) if out.status == 1 => "origin".to_string(),
        Ok(out) => return Err(format!("git config failed: {}", last_line(&out))),
        Err(e) => return Err(format!("couldn't run git: {e}")),
    };
    let url = match git(&["remote", "get-url", &name]) {
        Ok(out) if out.success() => out.stdout_string(),
        Ok(_) => return Err(format!("it has no remote named {name}")),
        Err(e) => return Err(format!("couldn't run git: {e}")),
    };
    Remote::parse(&url).ok_or_else(|| format!("the URL of {name} names no forge repository"))
}

/// Ask the forge about the branch checked out in `dir`.
pub fn lookup(
    p: &dyn Platform,
    dir: &Path,
    branch: &str,
    forges: &BTreeMap<String, ForgeKind>,
) -> ForgeState {
    match remote_of(p, dir, branch) {
        Ok(remote) => query(p, kind_for(&remote.host, forges), &remote, branch),
        Err(reason) => ForgeState::Unknown(reason),
    }
}

/// Ask `kind`'s REST API for every request from `branch` in `remote`.
///
/// * GitHub: `GET repos/{owner}/{repo}/pulls?head={owner}:{branch}&state=all`.
///   A pull request that merged is `closed` with a `merged_at`.
/// * GitLab: `GET projects/{path}/merge_requests?source_branch={branch}&state=all`,
///   whose `state` is `opened`, `closed`, `locked` or `merged`.
pub fn query(p: &dyn Platform, kind: ForgeKind, remote: &Remote, branch: &str) -> ForgeState {
    let (program, args) = match kind {
        ForgeKind::Github => {
            let Some((owner, _)) = remote.path.split_once('/') else {
                return ForgeState::Unknown(format!("{} is not owner/repo", remote.path));
            };
            let endpoint = format!(
                "repos/{}/pulls?head={}:{}&state=all&per_page=100",
                remote.path,
                encode(owner),
                encode(branch)
            );
            let version = format!("X-GitHub-Api-Version: {GITHUB_API_VERSION}");
            let args = vec![
                "api".to_string(),
                "--hostname".to_string(),
                remote.host.clone(),
                "-H".to_string(),
                "Accept: application/vnd.github+json".to_string(),
                "-H".to_string(),
                version,
                endpoint,
            ];
            ("gh", args)
        }
        ForgeKind::Gitlab => {
            let endpoint = format!(
                "projects/{}/merge_requests?source_branch={}&state=all&per_page=100",
                encode(&remote.path),
                encode(branch)
            );
            let args = vec![
                "api".to_string(),
                "--hostname".to_string(),
                remote.host.clone(),
                endpoint,
            ];
            ("glab", args)
        }
        ForgeKind::None => {
            return ForgeState::Unknown(format!("no forge is configured for {}", remote.host));
        }
    };
    match p.run_command(&CommandSpec::new(program, args).with_timeout(TIMEOUT)) {
        Err(e) if e.kind() == ErrorKind::NotFound => {
            ForgeState::Unknown(format!("{program} is not installed"))
        }
        Err(e) => ForgeState::Unknown(format!("couldn't run {program}: {e}")),
        Ok(out) if !out.success() => {
            ForgeState::Unknown(format!("{program} failed: {}", last_line(&out)))
        }
        Ok(out) => parse_requests(kind, &out.stdout_string()).unwrap_or_else(|| {
            ForgeState::Unknown(format!(
                "{program} returned something other than a list of requests"
            ))
        }),
    }
}

/// One request as either API returns it; each reads only its own fields.
#[derive(Deserialize)]
struct Listed {
    #[serde(alias = "iid")]
    number: u64,
    state: String,
    /// GitHub only: set once the pull request merged.
    #[serde(default)]
    merged_at: Option<String>,
}

/// What one request says about its branch.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Said {
    Open,
    Merged,
    Closed,
}

/// Reduce an API's JSON list of requests to one state: any open one wins, then
/// the newest merged one, then the newest closed one. A state neither API
/// documents counts as open, which keeps the worktree. `None` for anything
/// that is not such a list.
fn parse_requests(kind: ForgeKind, stdout: &str) -> Option<ForgeState> {
    let listed: Vec<Listed> = serde_json::from_str(stdout.trim()).ok()?;
    let said = |l: &Listed| match (kind, l.state.as_str()) {
        (ForgeKind::Github, "closed") if l.merged_at.is_some() => Said::Merged,
        (ForgeKind::Github, "closed") => Said::Closed,
        (ForgeKind::Gitlab, "merged") => Said::Merged,
        (ForgeKind::Gitlab, "closed") => Said::Closed,
        _ => Said::Open,
    };
    let newest = |want: Said| {
        listed
            .iter()
            .filter(|l| said(l) == want)
            .map(|l| l.number)
            .max()
            .map(|number| Request { kind, number })
    };
    Some(
        match (
            newest(Said::Open),
            newest(Said::Merged),
            newest(Said::Closed),
        ) {
            (Some(r), _, _) => ForgeState::Open(r),
            (None, Some(r), _) => ForgeState::Merged(r),
            (None, None, Some(r)) => ForgeState::Closed(r),
            (None, None, None) => ForgeState::NoRequest(kind),
        },
    )
}

/// Percent-encode everything but RFC 3986's unreserved characters, for a path
/// segment or a query value.
fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                char::from(b).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The last non-empty line a command wrote to stderr, shortened for a label.
fn last_line(out: &CommandOutput) -> String {
    let stderr = out.stderr_string();
    match stderr.lines().map(str::trim).rfind(|l| !l.is_empty()) {
        Some(line) => line.chars().take(200).collect(),
        None => format!("exit status {}", out.status),
    }
}

#[cfg(test)]
mod tests;
