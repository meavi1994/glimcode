//! Read-only Git operations. Every process is bounded and runs on a worker.
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChangedFile {
    pub path: PathBuf,
    pub old_path: Option<PathBuf>,
    pub committed: bool,
    pub uncommitted: bool,
    pub deleted: bool,
    pub added: bool,
    pub conflict: bool,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Remote {
    pub host: String,
    pub owner: String,
    pub name: String,
}
impl Remote {
    pub fn web(&self) -> String {
        format!("https://{}/{}/{}", self.host, self.owner, self.name)
    }
}
#[derive(Clone, Debug)]
pub struct Repository {
    pub root: PathBuf,
    pub branch: String,
    pub head: String,
    pub base: String,
    pub merge_base: Option<String>,
    pub branches: Vec<String>,
    pub files: Vec<ChangedFile>,
    pub remote: Option<Remote>,
}
#[derive(Clone, Debug, Default)]
pub struct BlameLine {
    pub commit: String,
    pub author: String,
    pub date: String,
    pub summary: String,
}
impl BlameLine {
    pub fn local(&self) -> bool {
        self.commit.is_empty() || self.commit.bytes().all(|b| b == b'0')
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mark {
    Added,
    Modified,
    Deleted,
}
#[derive(Debug)]
pub struct FileData {
    pub path: PathBuf,
    pub base: String,
    pub text: String,
    pub marks: Vec<(usize, Mark)>,
    pub change_lines: Vec<usize>,
    pub blame: Vec<BlameLine>,
    pub blame_note: Option<String>,
}
#[derive(Clone, Debug)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub body: String,
    pub author: String,
    pub state: String,
    pub base: String,
    pub head: String,
    pub updated: String,
}
#[derive(Clone, Debug)]
pub struct CommitDetail {
    pub sha: String,
    pub author: String,
    pub email: String,
    pub date: String,
    pub subject: String,
    pub body: String,
    pub url: Option<String>,
    pub prs: Vec<PullRequest>,
    pub pr_note: Option<String>,
}
struct Output {
    success: bool,
    bytes: Vec<u8>,
    error: String,
}

fn run(mut command: Command, input: Option<Vec<u8>>, timeout: Duration) -> Result<Output> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_PAGER", "cat");
    let mut child = command
        .spawn()
        .context("Could not start Git or GitHub CLI")?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut bytes = vec![];
        stdout
            .take(80 * 1024 * 1024)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let err = std::thread::spawn(move || {
        let mut bytes = vec![];
        stderr
            .take(1024 * 1024)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Git operation timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let bytes = out
        .join()
        .map_err(|_| anyhow::anyhow!("Git output reader failed"))??;
    let error = String::from_utf8_lossy(
        &err.join()
            .map_err(|_| anyhow::anyhow!("Git error reader failed"))??,
    )
    .trim()
    .chars()
    .take(2000)
    .collect();
    Ok(Output {
        success: status.success(),
        bytes,
        error,
    })
}
fn command(root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .args(["--no-pager", "--literal-pathspecs"]);
    command
}
fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut cmd = command(root);
    cmd.args(args);
    let output = run(cmd, None, Duration::from_secs(20))?;
    if !output.success {
        bail!("{}", output.error);
    }
    Ok(output.bytes)
}
fn text(root: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(git(root, args)?)?
        .trim_end_matches('\n')
        .to_owned())
}

pub fn remote(value: &str) -> Option<Remote> {
    let value = value.trim();
    let url = if value.contains("://") {
        url::Url::parse(value).ok()?
    } else {
        let (host, path) = value.split_once(':')?;
        url::Url::parse(&format!("ssh://{host}/{path}")).ok()?
    };
    let host = url.host_str()?.to_owned();
    let mut parts = url.path().trim_matches('/').split('/');
    let owner = parts.next()?.to_owned();
    let name = parts.next()?.trim_end_matches(".git").to_owned();
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(Remote { host, owner, name })
}

pub fn repository(project: &Path, requested_base: Option<&str>) -> Result<Repository> {
    let root = PathBuf::from(
        text(project, &["rev-parse", "--show-toplevel"])
            .context("This folder is not in a Git repository")?,
    );
    let head = text(&root, &["rev-parse", "--verify", "HEAD"]).unwrap_or_default();
    let branch = text(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .unwrap_or_else(|_| "(detached HEAD)".into());
    let branches: Vec<_> = text(
        &root,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads",
            "refs/remotes",
        ],
    )?
    .lines()
    .filter(|r| !r.ends_with("/HEAD"))
    .map(str::to_owned)
    .collect();
    let base = requested_base
        .filter(|b| branches.iter().any(|r| r == b))
        .map(str::to_owned)
        .or_else(|| {
            ["origin/main", "origin/master", "main", "master"]
                .into_iter()
                .find(|b| branches.iter().any(|r| r == b))
                .map(str::to_owned)
        })
        .or_else(|| branches.iter().find(|r| **r != branch).cloned())
        .unwrap_or_else(|| {
            if head.is_empty() {
                "Empty repository".into()
            } else {
                "HEAD".into()
            }
        });
    let merge_base = if head.is_empty() {
        None
    } else {
        Some(
            text(&root, &["merge-base", &base, "HEAD"])
                .context("The selected base has no common ancestor with this branch")?,
        )
    };
    let mut files = BTreeMap::new();
    if let Some(base) = &merge_base {
        parse_changes(
            &git(
                &root,
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--name-status",
                    "-z",
                    "--find-renames",
                    base,
                    "HEAD",
                    "--",
                ],
            )?,
            true,
            &mut files,
        );
    }
    parse_status(
        &git(
            &root,
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        )?,
        &mut files,
    );
    // A working-tree restoration of a committed deletion is no longer deleted.
    for file in files.values_mut() {
        file.deleted = !root.join(&file.path).exists() && !root.join(&file.path).is_symlink();
    }
    let remote = text(&root, &["remote", "get-url", "origin"])
        .ok()
        .and_then(|s| remote(&s));
    Ok(Repository {
        root,
        branch,
        head,
        base,
        merge_base,
        branches,
        files: files.into_values().collect(),
        remote,
    })
}
fn parse_changes(bytes: &[u8], committed: bool, files: &mut BTreeMap<PathBuf, ChangedFile>) {
    let fields: Vec<_> = bytes.split(|b| *b == 0).collect();
    let mut i = 0;
    while i + 1 < fields.len() {
        let status = fields[i].first().copied().unwrap_or(b'?');
        i += 1;
        let first = PathBuf::from(String::from_utf8_lossy(fields[i]).into_owned());
        i += 1;
        let (path, old) = if matches!(status, b'R' | b'C') && i < fields.len() {
            let path = PathBuf::from(String::from_utf8_lossy(fields[i]).into_owned());
            i += 1;
            (path, Some(first))
        } else {
            (first, None)
        };
        let file = files.entry(path.clone()).or_insert_with(|| ChangedFile {
            path,
            ..Default::default()
        });
        file.committed |= committed;
        file.deleted = status == b'D';
        file.added |= status == b'A';
        if old.is_some() && file.old_path.is_none() {
            file.old_path = old;
        }
    }
}
fn parse_status(bytes: &[u8], files: &mut BTreeMap<PathBuf, ChangedFile>) {
    let fields: Vec<_> = bytes.split(|b| *b == 0).collect();
    let mut i = 0;
    while i < fields.len() {
        let record = fields[i];
        i += 1;
        if record.len() < 4 {
            continue;
        }
        let status = &record[..2];
        let path = PathBuf::from(String::from_utf8_lossy(&record[3..]).into_owned());
        let old = if status.contains(&b'R') || status.contains(&b'C') {
            let old = fields
                .get(i)
                .map(|b| PathBuf::from(String::from_utf8_lossy(b).into_owned()));
            i += 1;
            old
        } else {
            None
        };
        if let Some(old) = &old
            && let Some(previous) = files.remove(old)
        {
            let file = files.entry(path.clone()).or_insert_with(|| ChangedFile {
                path: path.clone(),
                ..Default::default()
            });
            file.committed |= previous.committed;
            file.old_path = previous.old_path.or_else(|| Some(old.clone()));
        }
        let file = files.entry(path.clone()).or_insert_with(|| ChangedFile {
            path,
            ..Default::default()
        });
        file.uncommitted = true;
        file.deleted = status.contains(&b'D');
        file.added |= status == b"??" || status.contains(&b'A');
        file.conflict = status.contains(&b'U') || status == b"AA" || status == b"DD";
        if file.old_path.is_none() {
            file.old_path = old;
        }
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut ancestor = path;
    let mut tail = vec![];
    loop {
        if let Ok(mut canonical) = ancestor.canonicalize() {
            for component in tail.into_iter().rev() {
                canonical.push(component);
            }
            return canonical;
        }
        let (Some(name), Some(parent)) = (ancestor.file_name(), ancestor.parent()) else {
            return path.to_path_buf();
        };
        tail.push(name.to_os_string());
        ancestor = parent;
    }
}

pub fn file_data(
    repo: &Repository,
    path: &Path,
    buffer: Option<String>,
    blame: bool,
) -> Result<FileData> {
    let normalized = normalize_path(path);
    let relative = path
        .strip_prefix(&repo.root)
        .or_else(|_| normalized.strip_prefix(&repo.root))
        .context("File is outside this Git repository")?;
    let old = repo
        .files
        .iter()
        .find(|f| f.path == relative)
        .and_then(|f| f.old_path.as_deref())
        .unwrap_or(relative);
    let base = if let Some(base) = &repo.merge_base {
        let object = format!("{}:{}", base, old.to_string_lossy());
        // Missing base paths are newly added files; other Git failures remain visible.
        let exists = git(&repo.root, &["cat-file", "-e", &object]).is_ok();
        if exists {
            String::from_utf8(git(
                &repo.root,
                &["show", "--no-ext-diff", "--no-textconv", &object],
            )?)
            .context("Binary files cannot be displayed as text diffs")?
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    if base.len() as u64 > crate::navigation::MAX_FILE_BYTES {
        bail!("Base file exceeds the 20 MiB diff limit");
    }
    let current = match buffer {
        Some(text) => text,
        None if path.exists() => crate::navigation::read_text(path)?,
        None => String::new(),
    };
    if base.contains('\0') || current.contains('\0') {
        bail!("Binary file — text diff unavailable");
    }
    let marks = line_marks(&base, &current);
    let (blame, blame_note) = if blame {
        let head_path = format!("HEAD:{}", relative.to_string_lossy());
        let old_head_path = format!("HEAD:{}", old.to_string_lossy());
        let blame_path = if git(&repo.root, &["cat-file", "-e", &head_path]).is_ok() {
            Some(relative)
        } else if old != relative && git(&repo.root, &["cat-file", "-e", &old_head_path]).is_ok() {
            Some(old)
        } else {
            None
        };
        if let Some(blame_path) = blame_path {
            let mut command = command(&repo.root);
            command
                .args(["blame", "--line-porcelain", "--contents", "-", "--"])
                .arg(blame_path);
            match run(
                command,
                Some(current.as_bytes().to_vec()),
                Duration::from_secs(20),
            ) {
                Ok(output) if output.success => {
                    (parse_blame(&String::from_utf8_lossy(&output.bytes)), None)
                }
                Ok(output) => (vec![], Some(format!("Blame unavailable: {}", output.error))),
                Err(error) => (vec![], Some(error.to_string())),
            }
        } else {
            (
                current
                    .lines()
                    .map(|_| BlameLine {
                        author: "Uncommitted".into(),
                        summary: "New or untracked line".into(),
                        ..Default::default()
                    })
                    .collect(),
                Some("This file has not been committed.".into()),
            )
        }
    } else {
        (vec![], None)
    };
    let change_lines = crate::diff::change_lines(&base, &current);
    Ok(FileData {
        path: path.to_path_buf(),
        base,
        text: current,
        marks,
        change_lines,
        blame,
        blame_note,
    })
}

pub fn line_marks(base: &str, current: &str) -> Vec<(usize, Mark)> {
    let rows = crate::diff::compare(base, current);
    let mut result = vec![];
    let mut i = 0;
    let mut anchor = 0;
    while i < rows.len() {
        if !rows[i].changed {
            anchor = rows[i].editor.map_or(anchor, |n| n + 1);
            i += 1;
            continue;
        }
        let start = i;
        while i < rows.len() && rows[i].changed {
            i += 1;
        }
        let removed = rows[start..i].iter().filter(|r| r.disk.is_some()).count();
        let added: Vec<_> = rows[start..i].iter().filter_map(|r| r.editor).collect();
        if added.is_empty() {
            result.push((
                anchor.min(current.split('\n').count().saturating_sub(1)),
                Mark::Deleted,
            ));
        } else {
            for (index, line) in added.into_iter().enumerate() {
                result.push((
                    line,
                    if index < removed {
                        Mark::Modified
                    } else {
                        Mark::Added
                    },
                ));
                anchor = line + 1;
            }
        }
    }
    result
}

pub fn parse_blame(source: &str) -> Vec<BlameLine> {
    let mut result = vec![];
    let mut current = BlameLine::default();
    let mut line_number = 0;
    for line in source.lines() {
        let parts: Vec<_> = line.split_whitespace().collect();
        if parts.len() >= 3
            && parts[0].len() >= 40
            && parts[0].bytes().all(|b| b.is_ascii_hexdigit())
        {
            current = BlameLine {
                commit: parts[0].into(),
                ..Default::default()
            };
            line_number = parts[2].parse::<usize>().unwrap_or(1).saturating_sub(1);
        } else if let Some(author) = line.strip_prefix("author ") {
            current.author = author.into();
        } else if let Some(time) = line.strip_prefix("author-time ") {
            current.date = date(time.parse().unwrap_or(0));
        } else if let Some(summary) = line.strip_prefix("summary ") {
            current.summary = summary.into();
        } else if line.starts_with('\t') {
            if current.local() {
                current.author = "Uncommitted".into();
                current.date.clear();
            }
            result.resize_with(line_number + 1, BlameLine::default);
            result[line_number] = current.clone();
        }
    }
    result
}
fn date(seconds: i64) -> String {
    let z = seconds.div_euclid(86400) + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    y += i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn pull_request(value: &Value) -> Option<PullRequest> {
    let number = value["number"].as_u64()?;
    let string = |key: &str| value[key].as_str().unwrap_or("").to_owned();
    let author = value["author"]["login"]
        .as_str()
        .or_else(|| value["user"]["login"].as_str())
        .unwrap_or("")
        .into();
    let base = value["baseRefName"]
        .as_str()
        .or_else(|| value["base"]["ref"].as_str())
        .unwrap_or("")
        .into();
    let head = value["headRefName"]
        .as_str()
        .or_else(|| value["head"]["ref"].as_str())
        .unwrap_or("")
        .into();
    let url = value["url"]
        .as_str()
        .filter(|u| u.contains("/pull/"))
        .or_else(|| value["html_url"].as_str())
        .unwrap_or("")
        .into();
    Some(PullRequest {
        number,
        title: string("title"),
        url,
        body: string("body"),
        author,
        state: if value["merged_at"].is_string() || value["merged"].as_bool() == Some(true) {
            "merged".into()
        } else {
            string("state")
        },
        base,
        head,
        updated: value["updatedAt"]
            .as_str()
            .or_else(|| value["updated_at"].as_str())
            .unwrap_or("")
            .into(),
    })
}
fn github(repo: &Repository, endpoint: &str) -> Result<Value> {
    let remote = repo
        .remote
        .as_ref()
        .context("No GitHub origin remote is configured")?;
    let mut command = Command::new("gh");
    command
        .current_dir(&repo.root)
        .args(["api", "--hostname", &remote.host, endpoint]);
    let output = run(command, None, Duration::from_secs(15))?;
    if !output.success {
        bail!("GitHub lookup failed: {}", output.error);
    }
    Ok(serde_json::from_slice(&output.bytes)?)
}

pub fn branch_pr(repo: &Repository) -> Result<Option<PullRequest>> {
    if repo.branch == "(detached HEAD)" {
        return Ok(None);
    }
    let remote = repo
        .remote
        .as_ref()
        .context("No origin remote is configured")?;
    let mut url = url::Url::parse(&format!(
        "https://api.github.com/repos/{}/{}/pulls",
        remote.owner, remote.name
    ))?;
    url.query_pairs_mut()
        .append_pair("state", "open")
        .append_pair("head", &format!("{}:{}", remote.owner, repo.branch))
        .append_pair("per_page", "1");
    let endpoint = format!(
        "{}?{}",
        url.path().trim_start_matches('/'),
        url.query().unwrap_or("")
    );
    let value = github(repo, &endpoint)?;
    Ok(value
        .as_array()
        .and_then(|array| array.first())
        .and_then(pull_request))
}
pub fn commit_detail(repo: &Repository, blame: &BlameLine) -> Result<CommitDetail> {
    if blame.local() {
        return Ok(CommitDetail {
            sha: String::new(),
            author: "Uncommitted changes".into(),
            email: String::new(),
            date: String::new(),
            subject: "This line has not been committed".into(),
            body: "Commit and PR history becomes available after this line is committed.".into(),
            url: None,
            prs: vec![],
            pr_note: None,
        });
    }
    if !blame.commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("Invalid commit identifier");
    }
    let data = text(
        &repo.root,
        &[
            "show",
            "--no-patch",
            "--format=%H%x00%an%x00%ae%x00%aI%x00%s%x00%b",
            &blame.commit,
        ],
    )?;
    let parts: Vec<_> = data.splitn(6, '\0').collect();
    let get = |i| parts.get(i).copied().unwrap_or("").to_owned();
    let (prs, pr_note) = if let Some(remote) = &repo.remote {
        let endpoint = format!(
            "repos/{}/{}/commits/{}/pulls",
            remote.owner, remote.name, blame.commit
        );
        match github(repo, &endpoint) {
            Ok(value) => (
                value
                    .as_array()
                    .map(|a| a.iter().filter_map(pull_request).collect())
                    .unwrap_or_default(),
                None,
            ),
            Err(e) => (vec![], Some(e.to_string())),
        }
    } else {
        (
            vec![],
            Some("No origin remote is configured for PR lookup.".into()),
        )
    };
    Ok(CommitDetail {
        sha: get(0),
        author: get(1),
        email: get(2),
        date: get(3),
        subject: get(4),
        body: get(5),
        url: repo
            .remote
            .as_ref()
            .map(|r| format!("{}/commit/{}", r.web(), blame.commit)),
        prs,
        pr_note,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) fn setup() -> Result<tempfile::TempDir> {
        let dir = tempfile::tempdir()?;
        git(dir.path(), &["init", "-b", "main"])?;
        git(dir.path(), &["config", "user.name", "Test Author"])?;
        git(dir.path(), &["config", "user.email", "test@example.com"])?;
        std::fs::create_dir(dir.path().join("src"))?;
        std::fs::write(
            dir.path().join("src/code.rs"),
            "fn main() {\n    old();\n}\n",
        )?;
        std::fs::write(dir.path().join("deleted.txt"), "remove me\n")?;
        std::fs::write(dir.path().join("rename me.txt"), "rename content\n")?;
        git(dir.path(), &["add", "."])?;
        git(dir.path(), &["commit", "-m", "Base commit"])?;
        git(
            dir.path(),
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        )?;
        git(dir.path(), &["checkout", "-b", "feature"])?;
        Ok(dir)
    }
    #[test]
    fn changed_files_include_commits_working_tree_deletions_renames_and_untracked() -> Result<()> {
        let dir = setup()?;
        let root = dir.path();
        std::fs::write(
            root.join("src/code.rs"),
            "fn main() {\n    committed();\n}\n",
        )?;
        git(root, &["add", "src/code.rs"])?;
        git(root, &["commit", "-m", "Branch change"])?;
        std::fs::write(root.join("src/code.rs"), "fn main() {\n    local();\n}\n")?;
        std::fs::remove_file(root.join("deleted.txt"))?;
        git(root, &["mv", "rename me.txt", "renamed 猫.txt"])?;
        std::fs::write(root.join("untracked.txt"), "new file\n")?;
        let repo = repository(root, None)?;
        assert_eq!(repo.base, "origin/main");
        let file = repo
            .files
            .iter()
            .find(|f| f.path == Path::new("src/code.rs"))
            .unwrap();
        assert!(file.committed && file.uncommitted);
        assert!(
            repo.files
                .iter()
                .any(|f| f.path == Path::new("deleted.txt") && f.deleted && f.uncommitted)
        );
        assert!(
            repo.files
                .iter()
                .any(|f| f.path == Path::new("untracked.txt") && f.added && f.uncommitted)
        );
        let renamed = repo
            .files
            .iter()
            .find(|f| f.path == Path::new("renamed 猫.txt"))
            .unwrap();
        assert_eq!(
            renamed.old_path.as_deref(),
            Some(Path::new("rename me.txt"))
        );
        let data = file_data(&repo, &root.join("renamed 猫.txt"), None, true)?;
        assert!(
            !data.blame[0].local(),
            "Renames must preserve line authorship"
        );
        assert_eq!(data.base, "rename content\n");
        assert!(data.marks.is_empty());
        let deleted = file_data(&repo, &root.join("deleted.txt"), None, false)?;
        assert_eq!(deleted.base, "remove me\n");
        assert!(deleted.text.is_empty());
        Ok(())
    }
    #[test]
    fn merge_base_diff_and_blame_include_unsaved_editor_lines() -> Result<()> {
        let dir = setup()?;
        let root = dir.path();
        let base = text(root, &["rev-parse", "HEAD"])?;
        std::fs::write(
            root.join("src/code.rs"),
            "fn main() {\n    committed();\n}\n",
        )?;
        git(root, &["add", "."])?;
        git(root, &["commit", "-m", "Change function"])?;
        // Advancing the base independently must not appear as a branch-side deletion.
        git(root, &["checkout", "main"])?;
        std::fs::write(root.join("main-only.txt"), "base only\n")?;
        git(root, &["add", "."])?;
        git(root, &["commit", "-m", "Advance base"])?;
        git(root, &["update-ref", "refs/remotes/origin/main", "HEAD"])?;
        git(root, &["checkout", "feature"])?;
        let repo = repository(root, None)?;
        assert_eq!(repo.merge_base.as_deref(), Some(base.as_str()));
        assert!(
            !repo
                .files
                .iter()
                .any(|f| f.path == Path::new("main-only.txt"))
        );
        let buffer = "fn main() {\n    unsaved();\n    added();\n}\n";
        let data = file_data(&repo, &root.join("src/code.rs"), Some(buffer.into()), true)?;
        assert_eq!(data.blame.len(), 4);
        assert!(!data.blame[0].local());
        assert!(data.blame[1].local());
        assert_eq!(data.blame[0].author, "Test Author");
        assert!(data.marks.contains(&(1, Mark::Modified)));
        let original = file_data(&repo, &root.join("src/code.rs"), None, true)?;
        let detail = commit_detail(&repo, &original.blame[1])?;
        assert_eq!(detail.subject, "Change function");
        assert!(detail.pr_note.is_some());
        Ok(())
    }
    #[test]
    fn alternate_base_and_empty_repository_are_supported() -> Result<()> {
        let dir = setup()?;
        git(
            dir.path(),
            &["update-ref", "refs/remotes/origin/master", "main"],
        )?;
        assert_eq!(
            repository(dir.path(), Some("origin/master"))?.base,
            "origin/master"
        );
        let empty = tempfile::tempdir()?;
        git(empty.path(), &["init", "-b", "main"])?;
        std::fs::write(empty.path().join("first.txt"), "first\n")?;
        let repo = repository(empty.path(), None)?;
        assert!(repo.head.is_empty());
        assert!(repo.files[0].uncommitted);
        assert_eq!(
            file_data(&repo, &empty.path().join("first.txt"), None, true)?.marks,
            [(0, Mark::Added)]
        );
        Ok(())
    }
    #[test]
    fn line_marks_use_green_blue_and_deletion_anchors() {
        assert_eq!(line_marks("a\nb\n", "a\nnew\nb\n"), [(1, Mark::Added)]);
        assert_eq!(line_marks("a\nb\n", "a\nchanged\n"), [(1, Mark::Modified)]);
        assert_eq!(line_marks("a\nb\n", "a\n"), [(1, Mark::Deleted)]);
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(86400), "1970-01-02");
    }
    #[test]
    fn github_remote_and_pr_metadata_are_normalized() {
        for url in [
            "git@github.com:owner/repo.git",
            "https://github.com/owner/repo.git",
            "ssh://git@github.com/owner/repo.git",
        ] {
            assert_eq!(remote(url).unwrap().web(), "https://github.com/owner/repo");
        }
        let value = serde_json::json!({"number":42,"title":"Improve navigation","html_url":"https://github.com/owner/repo/pull/42","url":"https://api.github.com/repos/owner/repo/pulls/42","body":"Full **description**","user":{"login":"author"},"base":{"ref":"main"},"head":{"ref":"feature"},"state":"open"});
        let pr = pull_request(&value).unwrap();
        assert_eq!(pr.url, "https://github.com/owner/repo/pull/42");
        assert_eq!(pr.body, "Full **description**");
        assert_eq!(pr.base, "main");
    }
}
