use crate::release::{CommitKind, ReleasePlan};
use release_kthx_domain::PlannedCommit;

pub fn render_markdown(plan: &ReleasePlan) -> String {
    let version = plan.next_version.to_string();
    render_sections(
        Some(version.as_str()),
        plan.base_tag.as_deref(),
        &plan.commits,
    )
}

pub fn render_release_notes(base_ref: Option<&str>, commits: &[PlannedCommit]) -> String {
    render_sections(None, base_ref, commits)
}

fn render_sections(
    version_heading: Option<&str>,
    base_ref: Option<&str>,
    commits: &[PlannedCommit],
) -> String {
    let mut out = String::new();

    if let Some(version_heading) = version_heading {
        out.push_str(&format!("## {}\n\n", version_heading));
    }

    if let Some(base_ref) = base_ref {
        out.push_str(&format!("Changes since `{}`\n\n", base_ref));
    }

    append_section(&mut out, "Features", commits, CommitKind::Feature);
    append_section(&mut out, "Fixes", commits, CommitKind::Fix);
    append_section(&mut out, "Refactors", commits, CommitKind::Refactor);
    append_section(
        &mut out,
        "Documentation",
        commits,
        CommitKind::Documentation,
    );
    append_section(&mut out, "Chores", commits, CommitKind::Chore);
    append_section(&mut out, "Other", commits, CommitKind::Other);

    out
}

fn append_section(out: &mut String, title: &str, commits: &[PlannedCommit], kind: CommitKind) {
    let mut wrote_any = false;
    for commit in commits {
        if commit.kind == kind {
            if !wrote_any {
                out.push_str(&format!("### {}\n", title));
                wrote_any = true;
            }
            if commit.breaking {
                out.push_str(&format!(
                    "- {} ({}) **BREAKING**\n",
                    commit.subject,
                    short_hash(&commit.hash)
                ));
            } else {
                out.push_str(&format!(
                    "- {} ({})\n",
                    commit.subject,
                    short_hash(&commit.hash)
                ));
            }
        }
    }
    if wrote_any {
        out.push('\n');
    }
}

fn short_hash(hash: &str) -> &str {
    let max = std::cmp::min(7, hash.len());
    &hash[..max]
}

#[cfg(test)]
mod tests {
    use super::*;
    use release_kthx_domain::{BumpLevel, PlannedCommit};
    use semver::Version;

    #[test]
    fn renders_feature_section() {
        let plan = ReleasePlan {
            base_tag: Some("v0.1.0".to_string()),
            current_version: Version::parse("0.1.0").expect("valid semver"),
            next_version: Version::parse("0.2.0").expect("valid semver"),
            bump_level: BumpLevel::Minor,
            commits: vec![PlannedCommit {
                hash: "abcdef123456".to_string(),
                subject: "feat: add private release mode".to_string(),
                kind: CommitKind::Feature,
                breaking: false,
            }],
        };

        let text = render_markdown(&plan);
        assert!(text.contains("### Features"));
        assert!(text.contains("add private release mode"));
    }

    #[test]
    fn release_notes_skip_version_heading() {
        let commits = vec![PlannedCommit {
            hash: "abcdef123456".to_string(),
            subject: "fix: patch parser".to_string(),
            kind: CommitKind::Fix,
            breaking: false,
        }];

        let text = render_release_notes(Some("v0.1.0"), &commits);
        assert!(!text.starts_with("## "));
        assert!(text.contains("Changes since `v0.1.0`"));
        assert!(text.contains("### Fixes"));
    }
}
