use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;
use toml::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRecord {
    pub hash: String,
    pub subject: String,
    pub body: String,
    pub files: Vec<PathBuf>,
}

pub trait CommitHistoryService {
    fn latest_tag(&self, path: &Path) -> Result<Option<String>>;
    fn collect_commits(&self, path: &Path, from_tag: Option<&str>) -> Result<Vec<CommitRecord>>;
    fn tag_exists(&self, path: &Path, tag_name: &str) -> Result<bool>;
    fn find_version_commit(
        &self,
        path: &Path,
        manifest_path: &Path,
        version: &str,
    ) -> Result<Option<String>>;
    fn previous_version(
        &self,
        path: &Path,
        manifest_path: &Path,
        version: &str,
    ) -> Result<Option<String>>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CliCommitHistoryService;

impl CommitHistoryService for CliCommitHistoryService {
    fn latest_tag(&self, path: &Path) -> Result<Option<String>> {
        latest_tag(path)
    }

    fn collect_commits(&self, path: &Path, from_tag: Option<&str>) -> Result<Vec<CommitRecord>> {
        collect_commits(path, from_tag)
    }

    fn tag_exists(&self, path: &Path, tag_name: &str) -> Result<bool> {
        tag_exists(path, tag_name)
    }

    fn find_version_commit(
        &self,
        path: &Path,
        manifest_path: &Path,
        version: &str,
    ) -> Result<Option<String>> {
        find_version_commit(path, manifest_path, version)
    }

    fn previous_version(
        &self,
        path: &Path,
        manifest_path: &Path,
        version: &str,
    ) -> Result<Option<String>> {
        previous_version(path, manifest_path, version)
    }
}

fn run_git(path: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .with_context(|| format!("failed executing git {:?}", args))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {:?} failed: {}", args, stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn run_git_owned(path: &Path, args: &[String]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .with_context(|| format!("failed executing git {:?}", args))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {:?} failed: {}", args, stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn commit_files(path: &Path, hash: &str) -> Result<Vec<PathBuf>> {
    let args = vec![
        "show".to_string(),
        "--pretty=format:".to_string(),
        "--name-only".to_string(),
        hash.to_string(),
    ];
    let raw = run_git_owned(path, &args)?;
    Ok(raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect::<Vec<_>>())
}

pub fn latest_tag(path: &Path) -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("describe")
        .arg("--tags")
        .arg("--abbrev=0")
        .output()
        .with_context(|| format!("failed finding latest tag in {}", path.display()))?;

    if !output.status.success() {
        return Ok(None);
    }

    let tag = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if tag.is_empty() {
        Ok(None)
    } else {
        Ok(Some(tag))
    }
}

pub fn find_version_commit(
    path: &Path,
    manifest_path: &Path,
    version: &str,
) -> Result<Option<String>> {
    let needle = format!("version = \"{version}\"");
    let args = vec![
        "log".to_string(),
        "--pretty=format:%H".to_string(),
        "-S".to_string(),
        needle,
        "--".to_string(),
        manifest_path.to_string_lossy().to_string(),
    ];

    let raw = run_git_owned(path, &args)?;
    let hash = raw
        .lines()
        .next()
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    if hash.is_empty() {
        Ok(None)
    } else {
        Ok(Some(hash))
    }
}

pub fn previous_version(
    path: &Path,
    manifest_path: &Path,
    version: &str,
) -> Result<Option<String>> {
    let Some(current_commit) = find_version_commit(path, manifest_path, version)? else {
        return Ok(None);
    };

    let spec = format!("{}^:{}", current_commit, manifest_path.to_string_lossy());
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("show")
        .arg(&spec)
        .output()
        .with_context(|| format!("failed reading {} at {}", manifest_path.display(), spec))?;

    if !output.status.success() {
        return Ok(None);
    }

    manifest_package_version(&String::from_utf8_lossy(&output.stdout), manifest_path)
}

fn manifest_package_version(raw: &str, manifest_path: &Path) -> Result<Option<String>> {
    let value = raw
        .parse::<Value>()
        .with_context(|| format!("failed parsing {}", manifest_path.display()))?;

    Ok(value
        .get("package")
        .and_then(Value::as_table)
        .and_then(|package| package.get("version"))
        .and_then(Value::as_str)
        .map(str::to_string))
}

pub fn collect_commits(path: &Path, from_tag: Option<&str>) -> Result<Vec<CommitRecord>> {
    let mut args = vec![
        "log".to_string(),
        "--no-merges".to_string(),
        "--invert-grep".to_string(),
        "--grep=^chore(release):".to_string(),
        "--pretty=format:%H%x1f%s%x1f%b%x1e".to_string(),
    ];
    if let Some(tag) = from_tag {
        args.push(format!("{tag}..HEAD"));
    }

    let raw = run_git_owned(path, &args)?;
    let mut commits = Vec::new();

    for row in raw.split('\u{1e}') {
        let row = row.trim();
        if row.is_empty() {
            continue;
        }
        let mut parts = row.split('\u{1f}');
        let hash = parts.next().unwrap_or_default().trim().to_string();
        let subject = parts.next().unwrap_or_default().trim().to_string();
        let body = parts.next().unwrap_or_default().trim().to_string();
        let files = commit_files(path, &hash)?;

        if hash.is_empty() || subject.is_empty() {
            continue;
        }

        commits.push(CommitRecord {
            hash,
            subject,
            body,
            files,
        });
    }

    Ok(commits)
}

pub fn changed_files_between(path: &Path, before: &str, after: &str) -> Result<Vec<PathBuf>> {
    let args = vec![
        "diff".to_string(),
        "--name-only".to_string(),
        before.to_string(),
        after.to_string(),
    ];

    let raw = run_git_owned(path, &args)?;
    Ok(raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect::<Vec<_>>())
}

pub fn tag_exists(path: &Path, tag_name: &str) -> Result<bool> {
    let spec = format!("refs/tags/{tag_name}");
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .arg("--verify")
        .arg("--quiet")
        .arg(spec)
        .output()
        .with_context(|| format!("failed checking tag {tag_name}"))?;
    Ok(output.status.success())
}

pub fn create_annotated_tag(path: &Path, tag_name: &str, message: &str) -> Result<()> {
    if tag_exists(path, tag_name)? {
        bail!("tag {tag_name} already exists");
    }
    run_git(path, &["tag", "-a", tag_name, "-m", message])?;
    Ok(())
}

pub fn push_tag(path: &Path, tag_name: &str) -> Result<()> {
    run_git(path, &["push", "origin", tag_name])?;
    Ok(())
}

pub fn checkout_new_branch(path: &Path, branch: &str) -> Result<()> {
    run_git(path, &["checkout", "-B", branch])?;
    Ok(())
}

pub fn ensure_identity(path: &Path) -> Result<()> {
    let name_output = Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("config")
        .arg("user.name")
        .output()
        .context("failed reading git user.name")?;

    if !name_output.status.success()
        || String::from_utf8_lossy(&name_output.stdout)
            .trim()
            .is_empty()
    {
        run_git(path, &["config", "user.name", "release-kthx[bot]"])?;
    }

    let email_output = Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("config")
        .arg("user.email")
        .output()
        .context("failed reading git user.email")?;

    if !email_output.status.success()
        || String::from_utf8_lossy(&email_output.stdout)
            .trim()
            .is_empty()
    {
        run_git(
            path,
            &[
                "config",
                "user.email",
                "release-kthx[bot]@users.noreply.github.com",
            ],
        )?;
    }

    Ok(())
}

pub fn add_files(path: &Path, files: &[PathBuf]) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let mut args = vec!["add".to_string()];
    for file in files {
        args.push(file.to_string_lossy().to_string());
    }
    run_git_owned(path, &args)?;
    Ok(())
}

pub fn has_staged_changes(path: &Path) -> Result<bool> {
    let status = Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("diff")
        .arg("--cached")
        .arg("--quiet")
        .status()
        .context("failed checking staged changes")?;

    match status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => bail!("failed checking staged changes"),
    }
}

pub fn commit(path: &Path, message: &str) -> Result<()> {
    run_git(path, &["commit", "-m", message])?;
    Ok(())
}

pub fn push_branch(path: &Path, branch: &str) -> Result<()> {
    run_git(
        path,
        &[
            "push",
            "--force-with-lease",
            "--set-upstream",
            "origin",
            branch,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use tempfile::TempDir;

    fn run_git_ok(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("git command should run");

        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn run_git_output(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("git command should run");

        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );

        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_repo() -> TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        run_git_ok(dir.path(), &["init"]);
        run_git_ok(dir.path(), &["config", "user.name", "tester"]);
        run_git_ok(dir.path(), &["config", "user.email", "tester@example.com"]);
        dir
    }

    fn write_file(repo: &Path, relative: &str, content: &str) {
        let path = repo.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directories");
        }
        fs::write(path, content).expect("write file");
    }

    fn commit_files(
        repo: &Path,
        files: &[(&str, &str)],
        subject: &str,
        body: Option<&str>,
    ) -> String {
        for (relative, content) in files {
            write_file(repo, relative, content);
            run_git_ok(repo, &["add", relative]);
        }

        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(repo)
            .arg("commit")
            .arg("--no-gpg-sign")
            .arg("-m")
            .arg(subject);
        if let Some(body_text) = body {
            cmd.arg("-m").arg(body_text);
        }

        let output = cmd.output().expect("commit should run");
        assert!(
            output.status.success(),
            "commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        run_git_output(repo, &["rev-parse", "HEAD"])
    }

    #[test]
    fn parse_empty_log_output() {
        let mut commits = Vec::new();
        for row in "".split('\u{1e}') {
            let row = row.trim();
            if !row.is_empty() {
                commits.push(row.to_string());
            }
        }
        assert!(commits.is_empty());
    }

    #[test]
    fn collect_commits_reads_changed_files_from_real_repo() {
        let repo = init_repo();
        let repo_path = repo.path();

        commit_files(
            repo_path,
            &[("src/lib.rs", "pub fn one() {}\n")],
            "feat: add library",
            Some("domain change"),
        );

        commit_files(
            repo_path,
            &[
                ("crates/release-kthx-domain/src/lib.rs", "pub fn two() {}\n"),
                ("README.md", "# repo\n"),
            ],
            "fix: patch parser",
            None,
        );

        let commits = collect_commits(repo_path, None).expect("collect commits");
        assert_eq!(commits.len(), 2);

        let fix = commits
            .iter()
            .find(|c| c.subject == "fix: patch parser")
            .expect("fix commit exists");
        assert!(fix.files.contains(&PathBuf::from("README.md")));
        assert!(
            fix.files
                .contains(&PathBuf::from("crates/release-kthx-domain/src/lib.rs"))
        );

        let feat = commits
            .iter()
            .find(|c| c.subject == "feat: add library")
            .expect("feat commit exists");
        assert!(feat.files.contains(&PathBuf::from("src/lib.rs")));
        assert!(feat.body.contains("domain change"));
    }

    #[test]
    fn collect_commits_respects_tag_range_and_skips_release_commits() {
        let repo = init_repo();
        let repo_path = repo.path();

        commit_files(
            repo_path,
            &[("src/lib.rs", "pub fn baseline() {}\n")],
            "feat: baseline",
            None,
        );
        run_git_ok(repo_path, &["tag", "v0.1.0"]);

        commit_files(
            repo_path,
            &[("src/lib.rs", "pub fn baseline() { let _x = 1; }\n")],
            "fix: apply patch",
            None,
        );

        commit_files(
            repo_path,
            &[(
                "Cargo.toml",
                "[package]\nname=\"demo\"\nversion=\"0.1.1\"\n",
            )],
            "chore(release): v0.1.1",
            None,
        );

        let commits = collect_commits(repo_path, Some("v0.1.0")).expect("collect commits");
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].subject, "fix: apply patch");
        assert!(commits[0].files.contains(&PathBuf::from("src/lib.rs")));
    }

    #[test]
    fn latest_tag_detects_head_tag() {
        let repo = init_repo();
        let repo_path = repo.path();

        commit_files(
            repo_path,
            &[("src/lib.rs", "pub fn baseline() {}\n")],
            "feat: baseline",
            None,
        );
        run_git_ok(repo_path, &["tag", "v0.1.0"]);

        commit_files(
            repo_path,
            &[("src/lib.rs", "pub fn baseline() { let _x = 1; }\n")],
            "fix: patch",
            None,
        );
        run_git_ok(repo_path, &["tag", "v0.1.1"]);

        let tag = latest_tag(repo_path).expect("latest tag");
        assert_eq!(tag.as_deref(), Some("v0.1.1"));
    }

    #[test]
    fn changed_files_between_returns_expected_paths() {
        let repo = init_repo();
        let repo_path = repo.path();

        commit_files(
            repo_path,
            &[("src/lib.rs", "pub fn one() {}\n")],
            "feat: one",
            None,
        );
        let before = run_git_output(repo_path, &["rev-parse", "HEAD"]);

        commit_files(
            repo_path,
            &[
                ("src/lib.rs", "pub fn two() {}\n"),
                (
                    "Cargo.toml",
                    "[package]\nname=\"demo\"\nversion=\"0.1.0\"\n",
                ),
            ],
            "fix: two",
            None,
        );
        let after = run_git_output(repo_path, &["rev-parse", "HEAD"]);

        let files = changed_files_between(repo_path, &before, &after).expect("changed files");
        assert!(files.contains(&PathBuf::from("Cargo.toml")));
        assert!(files.contains(&PathBuf::from("src/lib.rs")));
    }

    #[test]
    fn previous_version_reads_parent_manifest_version() {
        let repo = init_repo();
        let repo_path = repo.path();

        commit_files(
            repo_path,
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
                ),
                ("src/lib.rs", "pub fn one() {}\n"),
            ],
            "feat: initial release",
            None,
        );

        commit_files(
            repo_path,
            &[(
                "Cargo.toml",
                "[package]\nname = \"demo\"\nversion = \"0.2.0\"\nedition = \"2024\"\n",
            )],
            "chore(release): demo v0.2.0",
            None,
        );

        let version = previous_version(repo_path, Path::new("Cargo.toml"), "0.2.0")
            .expect("previous version");
        assert_eq!(version.as_deref(), Some("0.1.0"));
    }
}
