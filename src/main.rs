mod changelog;
mod cli;
mod config;
mod event;
mod git;
mod github;
mod release;

use anyhow::{Result, bail};
use clap::Parser;
use cli::{Cli, Command};
use config::ReleaseKthxConfig;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Init { path, force } => {
            let destination = path.join("release-kthx.toml");
            config::init_config(&destination, force)?;
            println!("wrote {}", destination.display());
        }
        Command::Check { path } => {
            let cfg = load_config(&path)?;
            cfg.validate()?;

            let drifts =
                release::internal_dependency_drifts(&path, cfg.release.internal_dependency_policy)?;
            if drifts.is_empty() {
                println!(
                    "config valid and internal dependencies consistent: {}",
                    path.join("release-kthx.toml").display()
                );
            } else {
                println!("internal dependency drift detected:");
                for drift in &drifts {
                    let before = drift.old_requirement.as_deref().unwrap_or("<none>");
                    let after = drift.new_requirement.as_deref().unwrap_or("<removed>");
                    println!(
                        "- {}: {} ({}) {} -> {}",
                        drift.manifest_path.display(),
                        drift.dependency_key,
                        drift.dependency_name,
                        before,
                        after
                    );
                }
                bail!("internal dependency manifests require updates");
            }
        }
        Command::Plan { path, from_tag } => {
            run_plan(path, from_tag)?;
        }
        Command::ReleasePr {
            path,
            from_tag,
            base_branch,
            pr_branch,
        } => {
            run_release_pr(path, from_tag, &base_branch, &pr_branch)?;
        }
        Command::Release {
            path,
            from_tag,
            dry_run,
            push,
        } => {
            run_release(path, from_tag, dry_run, push)?;
        }
        Command::Publish {
            path,
            dry_run,
            push,
        } => {
            run_publish(path, dry_run, push)?;
        }
        Command::PublishOnMerge {
            path,
            dry_run,
            push,
        } => {
            run_publish_on_merge(path, dry_run, push)?;
        }
    }

    Ok(())
}

fn load_config(path: &Path) -> Result<ReleaseKthxConfig> {
    let config_path = path.join("release-kthx.toml");
    let cfg = ReleaseKthxConfig::from_path(&config_path)?;
    cfg.validate()?;
    Ok(cfg)
}

fn run_plan(path: PathBuf, from_tag: Option<String>) -> Result<()> {
    let cfg = load_config(&path)?;
    let plans =
        release::build_crate_release_plans(&path, from_tag.as_deref(), &cfg.release.tag_template)?;
    if plans.is_empty() {
        println!("no releasable changes detected");
        return Ok(());
    }

    println!("repo: {}", path.display());
    println!("crates-with-releases: {}", plans.len());
    println!(
        "github-release: {}",
        if cfg.github.create_release {
            "enabled"
        } else {
            "disabled"
        }
    );

    for crate_plan in &plans {
        println!();
        println!("crate: {}", crate_plan.crate_name);
        println!("manifest: {}", crate_plan.manifest_path.display());
        println!("base-version: {}", crate_plan.plan.current_version);
        println!("next-version: {}", crate_plan.plan.next_version);
        println!("bump: {}", crate_plan.plan.bump_level);
        println!("commits: {}", crate_plan.plan.commits.len());
        println!("{}", changelog::render_markdown(&crate_plan.plan));
    }

    Ok(())
}

fn run_release_pr(
    path: PathBuf,
    from_tag: Option<String>,
    base_branch: &str,
    pr_branch: &str,
) -> Result<()> {
    let cfg = load_config(&path)?;
    let plans =
        release::build_crate_release_plans(&path, from_tag.as_deref(), &cfg.release.tag_template)?;
    if plans.is_empty() {
        println!("no releasable changes detected; skipping release PR");
        return Ok(());
    }

    git::checkout_new_branch(&path, pr_branch)?;

    let mut changed_manifests = release::set_crate_versions(&path, &plans)?;
    changed_manifests.extend(release::set_internal_dependency_requirements(
        &path,
        cfg.release.internal_dependency_policy,
        &plans,
    )?);
    if release::set_lockfile_versions(&path, &plans)? {
        changed_manifests.push(PathBuf::from("Cargo.lock"));
    }
    changed_manifests.sort();
    changed_manifests.dedup();

    if changed_manifests.is_empty() {
        bail!("no Cargo.toml version fields found to update");
    }

    git::ensure_identity(&path)?;
    git::add_files(&path, &changed_manifests)?;

    let title = release_pr_title(&plans);
    if git::has_staged_changes(&path)? {
        git::commit(&path, &title)?;
    } else {
        println!(
            "release versions already up to date on branch {}",
            pr_branch
        );
    }

    git::push_branch(&path, pr_branch)?;

    let body = release_pr_body(&plans, &changed_manifests);
    let pr_url = github::create_or_update_release_pr(
        &path,
        &cfg.github.token_env,
        base_branch,
        pr_branch,
        &title,
        &body,
    )?;

    println!("release pr: {}", pr_url);
    Ok(())
}

fn release_pr_title(plans: &[release::CrateReleasePlan]) -> String {
    if plans.len() == 1 {
        let plan = &plans[0];
        return format!(
            "chore(release): {} v{}",
            plan.crate_name, plan.plan.next_version
        );
    }

    format!("chore(release): release {} crates", plans.len())
}

fn release_pr_body(plans: &[release::CrateReleasePlan], changed_manifests: &[PathBuf]) -> String {
    let mut body = String::new();
    body.push_str("## Release PR\n\n");
    body.push_str("## Crates\n");
    body.push_str("| Crate | Current | Next | Bump |\n");
    body.push_str("|---|---|---|---|\n");
    for crate_plan in plans {
        body.push_str(&format!(
            "| `{}` | `{}` | `{}` | `{}` |\n",
            crate_plan.crate_name,
            crate_plan.plan.current_version,
            crate_plan.plan.next_version,
            crate_plan.plan.bump_level
        ));
    }

    body.push_str("\n## Changed manifests\n");
    for manifest in changed_manifests {
        body.push_str(&format!("- `{}`\n", manifest.display()));
    }

    for crate_plan in plans {
        body.push_str(&format!("\n## {}\n\n", crate_plan.crate_name));
        body.push_str(&changelog::render_markdown(&crate_plan.plan));
    }

    body
}

fn run_release(path: PathBuf, from_tag: Option<String>, dry_run: bool, push: bool) -> Result<()> {
    let cfg = load_config(&path)?;
    let plans =
        release::build_crate_release_plans(&path, from_tag.as_deref(), &cfg.release.tag_template)?;
    if plans.is_empty() {
        println!("no releasable changes detected");
        return Ok(());
    }

    let crate_count = plans.len();
    let mut targets = Vec::new();
    for crate_plan in plans {
        let tag_name = release::render_tag_name(
            &cfg.release.tag_template,
            &crate_plan.crate_name,
            &crate_plan.plan.next_version,
            crate_count,
        )?;
        targets.push((crate_plan, tag_name));
    }

    println!("release plan:");
    for (crate_plan, tag_name) in &targets {
        println!(
            "- {} {} -> {} ({}) tag={}",
            crate_plan.crate_name,
            crate_plan.plan.current_version,
            crate_plan.plan.next_version,
            crate_plan.plan.bump_level,
            tag_name
        );
    }

    if dry_run {
        println!("dry-run enabled, no git tags created");
        return Ok(());
    }

    for (crate_plan, tag_name) in &targets {
        git::create_annotated_tag(
            &path,
            tag_name,
            &format!(
                "Release {} {}",
                crate_plan.crate_name, crate_plan.plan.next_version
            ),
        )?;
        println!("created tag {}", tag_name);
        if push {
            git::push_tag(&path, tag_name)?;
            println!("pushed tag {} to origin", tag_name);
        }
    }

    if cfg.github.create_release {
        println!(
            "github release enabled in config. publish mode will create per-crate releases using token env {}.",
            cfg.github.token_env
        );
    }
    Ok(())
}

fn run_publish(path: PathBuf, dry_run: bool, push: bool) -> Result<()> {
    let cfg = load_config(&path)?;
    let crates = release::collect_crates(&path)?;
    if crates.is_empty() {
        bail!("no Cargo package manifests found");
    }
    let notes_by_crate = release::build_crate_release_notes(&path, &cfg.release.tag_template)?
        .into_iter()
        .map(|notes| {
            (
                notes.crate_name,
                changelog::render_release_notes(notes.base_ref.as_deref(), &notes.commits),
            )
        })
        .collect::<BTreeMap<_, _>>();

    let crate_count = crates.len();
    let mut pending = Vec::new();
    for crate_info in crates {
        let tag_name = release::render_tag_name(
            &cfg.release.tag_template,
            &crate_info.name,
            &crate_info.version,
            crate_count,
        )?;

        if git::tag_exists(&path, &tag_name)? {
            continue;
        }

        pending.push((crate_info, tag_name));
    }

    if pending.is_empty() {
        println!("publish plan: no new crate versions to tag or release");
        return Ok(());
    }

    println!("publish plan:");
    for (crate_info, tag_name) in &pending {
        println!(
            "- crate: {} version: {} tag: {}",
            crate_info.name, crate_info.version, tag_name
        );
    }

    if dry_run {
        println!("dry-run enabled, no tag or github release created");
        return Ok(());
    }

    for (crate_info, tag_name) in &pending {
        git::create_annotated_tag(
            &path,
            tag_name,
            &format!("Release {} {}", crate_info.name, crate_info.version),
        )?;
        println!("created tag {}", tag_name);

        if push {
            git::push_tag(&path, tag_name)?;
            println!("pushed tag {} to origin", tag_name);
        }

        if cfg.github.create_release {
            let title = format!("Release {} {}", crate_info.name, crate_info.version);
            let notes = notes_by_crate
                .get(&crate_info.name)
                .cloned()
                .unwrap_or_else(|| {
                    format!(
                        "Automated publish for crate `{}` at version `{}`.",
                        crate_info.name, crate_info.version
                    )
                });
            let release_url = github::create_or_update_release(
                &path,
                &cfg.github.token_env,
                tag_name,
                &title,
                &notes,
            )?;
            println!("github release: {}", release_url);
        }
    }

    Ok(())
}

fn run_publish_on_merge(path: PathBuf, dry_run: bool, push: bool) -> Result<()> {
    let Some((before, after)) = event::push_range_from_env()? else {
        println!("publish-on-merge: skipped (not a push merge payload)");
        return Ok(());
    };

    let changed_files = git::changed_files_between(&path, &before, &after)?;
    if changed_files.is_empty() {
        println!("publish-on-merge: skipped (no changed files)");
        return Ok(());
    }

    if !release::is_release_merge_payload(&changed_files) {
        println!("publish-on-merge: skipped (changes are not release payload)");
        return Ok(());
    }

    run_publish(path, dry_run, push)
}
