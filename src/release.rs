use crate::git::{CliCommitHistoryService, CommitHistoryService};
use anyhow::{Context, Result, bail};
pub use release_kthx_domain::{CommitKind, ReleasePlan};
use release_kthx_domain::{
    DependencyOwner, DependencySource, InternalDependencyContext, InternalDependencyPolicy,
    PlannedCommit, Publication, RequirementStyle, WorkspaceCrate, WorkspaceGraph,
    desired_requirement_style,
};
use semver::Version;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use toml::Value;
use toml_edit::{DocumentMut, Item, TableLike, value};

#[derive(Debug, Clone)]
pub struct CrateInfo {
    pub name: String,
    pub manifest_path: PathBuf,
    pub crate_dir: PathBuf,
    pub version: Version,
    pub private: bool,
    pub local_dependencies: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CrateReleasePlan {
    pub crate_name: String,
    pub manifest_path: PathBuf,
    pub plan: ReleasePlan,
}

#[derive(Debug, Clone)]
pub struct CrateReleaseNotes {
    pub crate_name: String,
    pub manifest_path: PathBuf,
    pub base_ref: Option<String>,
    pub commits: Vec<PlannedCommit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalDependencyEdit {
    pub manifest_path: PathBuf,
    pub dependency_key: String,
    pub dependency_name: String,
    pub old_requirement: Option<String>,
    pub new_requirement: Option<String>,
}

pub fn collect_crates(path: &Path) -> Result<Vec<CrateInfo>> {
    let mut manifests = Vec::new();
    collect_cargo_manifests(path, &mut manifests)?;

    let mut crates = Vec::new();
    for manifest in manifests {
        if let Some(info) = parse_crate_info(path, &manifest)? {
            crates.push(info);
        }
    }

    crates.sort_by(|a, b| a.manifest_path.cmp(&b.manifest_path));
    populate_local_dependencies(path, &mut crates)?;
    Ok(crates)
}

pub fn build_crate_release_plans(
    path: &Path,
    from_tag: Option<&str>,
    tag_template: &str,
) -> Result<Vec<CrateReleasePlan>> {
    let history = CliCommitHistoryService;
    build_crate_release_plans_with_history(&history, path, from_tag, tag_template)
}

pub fn build_crate_release_plans_with_history(
    history: &impl CommitHistoryService,
    path: &Path,
    from_tag: Option<&str>,
    tag_template: &str,
) -> Result<Vec<CrateReleasePlan>> {
    let crates = collect_crates(path)?;
    if crates.is_empty() {
        bail!("no Cargo package manifests found");
    }
    let workspace_graph =
        WorkspaceGraph::from_crates(crates.iter().map(|crate_info| WorkspaceCrate {
            name: crate_info.name.clone(),
            local_dependencies: crate_info.local_dependencies.iter().cloned().collect(),
        }));

    let crate_count = crates.len();
    let mut plans = Vec::new();
    for crate_info in crates.iter() {
        let base_ref = resolve_base_reference(
            history,
            path,
            from_tag,
            tag_template,
            crate_info,
            crate_count,
        )?;

        let raw_commits = history.collect_commits(path, base_ref.as_deref())?;
        if raw_commits.is_empty() {
            continue;
        }

        let commit_inputs =
            crate_commit_inputs(raw_commits, &crates, &workspace_graph, &crate_info.name);

        let Some(plan) = release_kthx_domain::plan_release(
            crate_info.version.clone(),
            base_ref.clone(),
            commit_inputs,
        ) else {
            continue;
        };

        plans.push(CrateReleasePlan {
            crate_name: crate_info.name.clone(),
            manifest_path: crate_info.manifest_path.clone(),
            plan,
        });
    }

    plans.sort_by(|a, b| a.manifest_path.cmp(&b.manifest_path));
    Ok(plans)
}

pub fn build_crate_release_notes(
    path: &Path,
    tag_template: &str,
) -> Result<Vec<CrateReleaseNotes>> {
    let history = CliCommitHistoryService;
    build_crate_release_notes_with_history(&history, path, tag_template)
}

pub fn build_crate_release_notes_with_history(
    history: &impl CommitHistoryService,
    path: &Path,
    tag_template: &str,
) -> Result<Vec<CrateReleaseNotes>> {
    let crates = collect_crates(path)?;
    if crates.is_empty() {
        bail!("no Cargo package manifests found");
    }
    let workspace_graph =
        WorkspaceGraph::from_crates(crates.iter().map(|crate_info| WorkspaceCrate {
            name: crate_info.name.clone(),
            local_dependencies: crate_info.local_dependencies.iter().cloned().collect(),
        }));

    let crate_count = crates.len();
    let mut notes = Vec::new();
    for crate_info in crates.iter() {
        let base_ref =
            resolve_previous_base_reference(history, path, tag_template, crate_info, crate_count)?;

        let raw_commits = history.collect_commits(path, base_ref.as_deref())?;
        if raw_commits.is_empty() {
            continue;
        }

        let commits = crate_commit_inputs(raw_commits, &crates, &workspace_graph, &crate_info.name)
            .into_iter()
            .map(|input| PlannedCommit::from_input(input).0)
            .collect::<Vec<_>>();
        if commits.is_empty() {
            continue;
        }

        notes.push(CrateReleaseNotes {
            crate_name: crate_info.name.clone(),
            manifest_path: crate_info.manifest_path.clone(),
            base_ref,
            commits,
        });
    }

    notes.sort_by(|a, b| a.manifest_path.cmp(&b.manifest_path));
    Ok(notes)
}

fn resolve_base_reference(
    history: &impl CommitHistoryService,
    path: &Path,
    from_tag: Option<&str>,
    tag_template: &str,
    crate_info: &CrateInfo,
    crate_count: usize,
) -> Result<Option<String>> {
    if let Some(explicit) = from_tag {
        return Ok(Some(explicit.to_string()));
    }

    let expected_tag = render_tag_name(
        tag_template,
        &crate_info.name,
        &crate_info.version,
        crate_count,
    )?;

    if history.tag_exists(path, &expected_tag)? {
        return Ok(Some(expected_tag));
    }

    let version = crate_info.version.to_string();
    if let Some(commit) = history.find_version_commit(path, &crate_info.manifest_path, &version)? {
        return Ok(Some(commit));
    }

    history.latest_tag(path)
}

fn resolve_previous_base_reference(
    history: &impl CommitHistoryService,
    path: &Path,
    tag_template: &str,
    crate_info: &CrateInfo,
    crate_count: usize,
) -> Result<Option<String>> {
    let Some(previous_version) = history.previous_version(
        path,
        &crate_info.manifest_path,
        &crate_info.version.to_string(),
    )?
    else {
        return history.latest_tag(path);
    };

    let previous_version = Version::parse(&previous_version).with_context(|| {
        format!(
            "invalid previous version for {} in {}",
            crate_info.name,
            crate_info.manifest_path.display()
        )
    })?;

    let expected_tag = render_tag_name(
        tag_template,
        &crate_info.name,
        &previous_version,
        crate_count,
    )?;
    if history.tag_exists(path, &expected_tag)? {
        return Ok(Some(expected_tag));
    }

    if let Some(commit) = history.find_version_commit(
        path,
        &crate_info.manifest_path,
        &previous_version.to_string(),
    )? {
        return Ok(Some(commit));
    }

    history.latest_tag(path)
}

fn crate_commit_inputs(
    raw_commits: Vec<crate::git::CommitRecord>,
    crates: &[CrateInfo],
    workspace_graph: &WorkspaceGraph,
    crate_name: &str,
) -> Vec<release_kthx_domain::CommitInput> {
    let mut commit_inputs = Vec::new();

    for commit in raw_commits {
        let directly_affected = directly_affected_crates(crates, &commit.files);
        let topology = workspace_graph.release_topology(directly_affected.iter());
        if !topology.includes(crate_name) {
            continue;
        }

        commit_inputs.push(release_kthx_domain::CommitInput {
            hash: commit.hash,
            subject: commit.subject,
            body: commit.body,
        });
    }

    commit_inputs
}

pub fn set_crate_versions(path: &Path, plans: &[CrateReleasePlan]) -> Result<Vec<PathBuf>> {
    let mut changed = Vec::new();
    for plan in plans {
        let manifest_abs = path.join(&plan.manifest_path);
        if set_manifest_version(&manifest_abs, &plan.plan.next_version)? {
            changed.push(plan.manifest_path.clone());
        }
    }
    changed.sort();
    changed.dedup();
    Ok(changed)
}

pub fn set_internal_dependency_requirements(
    path: &Path,
    policy: InternalDependencyPolicy,
    plans: &[CrateReleasePlan],
) -> Result<Vec<PathBuf>> {
    let crates = collect_crates(path)?;
    let desired_versions = desired_versions_by_crate(&crates, plans);
    let edits =
        rewrite_internal_dependency_requirements(path, &crates, &desired_versions, policy, true)?;

    let mut changed = edits
        .into_iter()
        .map(|edit| edit.manifest_path)
        .collect::<Vec<_>>();
    changed.sort();
    changed.dedup();
    Ok(changed)
}

pub fn internal_dependency_drifts(
    path: &Path,
    policy: InternalDependencyPolicy,
) -> Result<Vec<InternalDependencyEdit>> {
    let crates = collect_crates(path)?;
    let desired_versions = desired_versions_by_crate(&crates, &[]);
    rewrite_internal_dependency_requirements(path, &crates, &desired_versions, policy, false)
}

pub fn set_lockfile_versions(path: &Path, plans: &[CrateReleasePlan]) -> Result<bool> {
    let lock_path = path.join("Cargo.lock");
    if !lock_path.exists() {
        return Ok(false);
    }

    let original = fs::read_to_string(&lock_path)
        .with_context(|| format!("failed reading {}", lock_path.display()))?;
    let mut doc = original
        .parse::<DocumentMut>()
        .with_context(|| format!("failed parsing {}", lock_path.display()))?;

    let mut changed = false;

    let package_item = match doc.get_mut("package") {
        Some(item) => item,
        None => return Ok(false),
    };
    let Some(packages) = package_item.as_array_of_tables_mut() else {
        return Ok(false);
    };

    for package in packages.iter_mut() {
        let Some(name) = package.get("name").and_then(|item| item.as_str()) else {
            continue;
        };

        let Some(plan) = plans.iter().find(|plan| plan.crate_name == name) else {
            continue;
        };

        let next_version = plan.plan.next_version.to_string();
        let current = package.get("version").and_then(|item| item.as_str());
        if current == Some(next_version.as_str()) {
            continue;
        }

        package.insert("version", value(next_version));
        changed = true;
    }

    if !changed {
        return Ok(false);
    }

    let rendered = doc.to_string();
    if rendered == original {
        return Ok(false);
    }

    fs::write(&lock_path, rendered)
        .with_context(|| format!("failed writing {}", lock_path.display()))?;
    Ok(true)
}

#[derive(Debug, Clone, Copy)]
enum DependencyRewriteScope<'a> {
    Member { dependent: Option<&'a CrateInfo> },
    WorkspaceDependencies,
}

fn desired_versions_by_crate(
    crates: &[CrateInfo],
    plans: &[CrateReleasePlan],
) -> BTreeMap<String, Version> {
    let mut desired = crates
        .iter()
        .map(|crate_info| (crate_info.name.clone(), crate_info.version.clone()))
        .collect::<BTreeMap<_, _>>();

    for plan in plans {
        desired.insert(plan.crate_name.clone(), plan.plan.next_version.clone());
    }

    desired
}

fn rewrite_internal_dependency_requirements(
    path: &Path,
    crates: &[CrateInfo],
    desired_versions: &BTreeMap<String, Version>,
    policy: InternalDependencyPolicy,
    write_changes: bool,
) -> Result<Vec<InternalDependencyEdit>> {
    let crates_by_name = crates
        .iter()
        .map(|crate_info| (crate_info.name.clone(), crate_info))
        .collect::<BTreeMap<_, _>>();
    let all_private = crates.iter().all(|crate_info| crate_info.private);

    let mut manifests = crates
        .iter()
        .map(|crate_info| crate_info.manifest_path.clone())
        .collect::<Vec<_>>();
    let root_manifest = PathBuf::from("Cargo.toml");
    if path.join(&root_manifest).exists() {
        manifests.push(root_manifest);
    }
    manifests.sort();
    manifests.dedup();

    let mut edits = Vec::new();
    for manifest_path in manifests {
        let manifest_abs = path.join(&manifest_path);
        let original = fs::read_to_string(&manifest_abs)
            .with_context(|| format!("failed reading {}", manifest_abs.display()))?;
        let mut doc = original
            .parse::<DocumentMut>()
            .with_context(|| format!("failed parsing {}", manifest_abs.display()))?;

        let dependent_name = manifest_package_name(&doc).map(str::to_string);
        let dependent = dependent_name
            .as_deref()
            .and_then(|name| crates_by_name.get(name).copied());

        let mut manifest_edits = Vec::new();

        for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(item) = doc.get_mut(section)
                && let Some(table) = item.as_table_like_mut()
            {
                rewrite_dependency_table(
                    table,
                    &manifest_path,
                    DependencyRewriteScope::Member { dependent },
                    &crates_by_name,
                    desired_versions,
                    policy,
                    all_private,
                    &mut manifest_edits,
                )?;
            }
        }

        if let Some(targets) = doc.get_mut("target")
            && let Some(targets_table) = targets.as_table_like_mut()
        {
            for (_, target_item) in targets_table.iter_mut() {
                let Some(target_table) = target_item.as_table_like_mut() else {
                    continue;
                };

                for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
                    if let Some(item) = target_table.get_mut(section)
                        && let Some(table) = item.as_table_like_mut()
                    {
                        rewrite_dependency_table(
                            table,
                            &manifest_path,
                            DependencyRewriteScope::Member { dependent },
                            &crates_by_name,
                            desired_versions,
                            policy,
                            all_private,
                            &mut manifest_edits,
                        )?;
                    }
                }
            }
        }

        if let Some(workspace) = doc.get_mut("workspace")
            && let Some(workspace_table) = workspace.as_table_like_mut()
            && let Some(item) = workspace_table.get_mut("dependencies")
            && let Some(table) = item.as_table_like_mut()
        {
            rewrite_dependency_table(
                table,
                &manifest_path,
                DependencyRewriteScope::WorkspaceDependencies,
                &crates_by_name,
                desired_versions,
                policy,
                all_private,
                &mut manifest_edits,
            )?;
        }

        if manifest_edits.is_empty() {
            continue;
        }

        let rendered = doc.to_string();
        if rendered == original {
            continue;
        }

        if write_changes {
            fs::write(&manifest_abs, rendered)
                .with_context(|| format!("failed writing {}", manifest_abs.display()))?;
        }

        edits.extend(manifest_edits);
    }

    edits.sort_by(|a, b| {
        a.manifest_path
            .cmp(&b.manifest_path)
            .then(a.dependency_key.cmp(&b.dependency_key))
            .then(a.dependency_name.cmp(&b.dependency_name))
    });
    Ok(edits)
}

fn manifest_package_name(doc: &DocumentMut) -> Option<&str> {
    doc.get("package")
        .and_then(Item::as_table_like)
        .and_then(|table| table.get("name"))
        .and_then(Item::as_str)
}

#[allow(clippy::too_many_arguments)]
fn rewrite_dependency_table(
    table: &mut dyn TableLike,
    manifest_path: &Path,
    scope: DependencyRewriteScope<'_>,
    crates_by_name: &BTreeMap<String, &CrateInfo>,
    desired_versions: &BTreeMap<String, Version>,
    policy: InternalDependencyPolicy,
    all_private: bool,
    edits: &mut Vec<InternalDependencyEdit>,
) -> Result<()> {
    for (key, item) in table.iter_mut() {
        let dependency_key = key.get().to_string();
        let Some(dependency_table) = item.as_table_like_mut() else {
            continue;
        };

        let dependency_name = dependency_table
            .get("package")
            .and_then(Item::as_str)
            .unwrap_or(dependency_key.as_str())
            .to_string();
        let Some(dependency_crate) = crates_by_name.get(&dependency_name).copied() else {
            continue;
        };
        let Some(source) = dependency_source(dependency_table) else {
            continue;
        };

        let desired_version = desired_versions
            .get(&dependency_name)
            .with_context(|| format!("missing desired version for {dependency_name}"))?;
        let current_requirement = dependency_table
            .get("version")
            .and_then(Item::as_str)
            .map(str::to_string);
        let current_style = current_requirement
            .as_deref()
            .map(RequirementStyle::parse)
            .transpose()
            .with_context(|| {
                format!(
                    "failed parsing internal dependency requirement for {} in {}",
                    dependency_name,
                    manifest_path.display()
                )
            })?;
        let next_requirement = desired_requirement_style(
            policy,
            InternalDependencyContext {
                owner: dependency_owner(scope),
                source,
                dependency_publication: publication(dependency_crate),
                all_members_private: all_private,
            },
            current_style,
        )
        .map(|style| style.render(desired_version));

        if current_requirement == next_requirement {
            continue;
        }

        apply_dependency_requirement(dependency_table, next_requirement.clone());
        edits.push(InternalDependencyEdit {
            manifest_path: manifest_path.to_path_buf(),
            dependency_key,
            dependency_name,
            old_requirement: current_requirement,
            new_requirement: next_requirement,
        });
    }

    Ok(())
}

fn dependency_source(table: &dyn TableLike) -> Option<DependencySource> {
    if table.contains_key("path") {
        Some(DependencySource::Path)
    } else if matches!(table.get("workspace").and_then(Item::as_bool), Some(true)) {
        Some(DependencySource::Workspace)
    } else {
        None
    }
}

fn dependency_owner(scope: DependencyRewriteScope<'_>) -> DependencyOwner {
    match scope {
        DependencyRewriteScope::Member {
            dependent: Some(dependent),
        } => DependencyOwner::Member {
            publication: publication(dependent),
        },
        DependencyRewriteScope::Member { dependent: None } => DependencyOwner::UnknownMember,
        DependencyRewriteScope::WorkspaceDependencies => DependencyOwner::Workspace,
    }
}

fn publication(crate_info: &CrateInfo) -> Publication {
    Publication::from_private(crate_info.private)
}

fn apply_dependency_requirement(table: &mut dyn TableLike, requirement: Option<String>) {
    match requirement {
        Some(requirement) => {
            table.insert("version", value(requirement));
        }
        None => {
            table.remove("version");
        }
    }
}

pub fn render_tag_name(
    tag_template: &str,
    crate_name: &str,
    version: &Version,
    crate_count: usize,
) -> Result<String> {
    if crate_count > 1 && !tag_template.contains("{{ crate }}") {
        bail!("release.tag_template must include '{{ crate }}' when multiple crates are released");
    }

    Ok(tag_template
        .replace("{{ crate }}", crate_name)
        .replace("{{ version }}", &version.to_string()))
}

pub fn is_release_merge_payload(files: &[PathBuf]) -> bool {
    let mut has_manifest = false;

    for file in files {
        let as_string = file.to_string_lossy();

        if as_string.ends_with("Cargo.toml") {
            has_manifest = true;
            continue;
        }

        if as_string == "Cargo.lock" {
            continue;
        }

        return false;
    }

    has_manifest
}

fn parse_crate_info(repo_root: &Path, manifest_abs: &Path) -> Result<Option<CrateInfo>> {
    let raw = fs::read_to_string(manifest_abs)
        .with_context(|| format!("failed reading {}", manifest_abs.display()))?;
    let value = raw
        .parse::<Value>()
        .with_context(|| format!("failed parsing {}", manifest_abs.display()))?;

    let Some(package) = value.get("package") else {
        return Ok(None);
    };
    let Some(name) = package.get("name").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some(version_str) = package.get("version").and_then(Value::as_str) else {
        return Ok(None);
    };

    let version = Version::parse(version_str)
        .with_context(|| format!("invalid semver version in {}", manifest_abs.display()))?;
    let private = matches!(package.get("publish").and_then(Value::as_bool), Some(false));

    let manifest_path = manifest_abs
        .strip_prefix(repo_root)
        .unwrap_or(manifest_abs)
        .to_path_buf();

    let crate_dir_abs = manifest_abs.parent().unwrap_or(repo_root);
    let crate_dir = if crate_dir_abs == repo_root {
        PathBuf::from(".")
    } else {
        crate_dir_abs
            .strip_prefix(repo_root)
            .unwrap_or(crate_dir_abs)
            .to_path_buf()
    };

    Ok(Some(CrateInfo {
        name: name.to_string(),
        manifest_path,
        crate_dir,
        version,
        private,
        local_dependencies: Vec::new(),
    }))
}

fn populate_local_dependencies(repo_root: &Path, crates: &mut [CrateInfo]) -> Result<()> {
    let crate_names = crates
        .iter()
        .map(|crate_info| crate_info.name.clone())
        .collect::<HashSet<_>>();

    for crate_info in crates.iter_mut() {
        let manifest_abs = repo_root.join(&crate_info.manifest_path);
        let raw = fs::read_to_string(&manifest_abs)
            .with_context(|| format!("failed reading {}", manifest_abs.display()))?;
        let value = raw
            .parse::<Value>()
            .with_context(|| format!("failed parsing {}", manifest_abs.display()))?;

        let mut deps = BTreeSet::new();
        for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(table) = value.get(section).and_then(Value::as_table) {
                collect_local_dependency_names(table, &crate_names, &mut deps);
            }
        }

        if let Some(targets) = value.get("target").and_then(Value::as_table) {
            for target in targets.values().filter_map(Value::as_table) {
                for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
                    if let Some(table) = target.get(section).and_then(Value::as_table) {
                        collect_local_dependency_names(table, &crate_names, &mut deps);
                    }
                }
            }
        }

        deps.remove(crate_info.name.as_str());
        crate_info.local_dependencies = deps.into_iter().collect();
    }

    Ok(())
}

fn collect_local_dependency_names(
    dependency_table: &toml::map::Map<String, Value>,
    crate_names: &HashSet<String>,
    result: &mut BTreeSet<String>,
) {
    for (key, value) in dependency_table {
        let crate_name = dependency_crate_name(key, value);
        if crate_names.contains(crate_name) {
            result.insert(crate_name.to_string());
        }
    }
}

fn dependency_crate_name<'a>(dependency_key: &'a str, dependency_value: &'a Value) -> &'a str {
    dependency_value
        .as_table()
        .and_then(|table| table.get("package"))
        .and_then(Value::as_str)
        .unwrap_or(dependency_key)
}

fn directly_affected_crates(crates: &[CrateInfo], files: &[PathBuf]) -> Vec<String> {
    let mut affected = HashSet::new();

    for file in files {
        let mut winner: Option<(usize, usize)> = None;

        for (index, crate_info) in crates.iter().enumerate() {
            if !crate_contains_file(&crate_info.crate_dir, file) {
                continue;
            }

            let rank = if crate_info.crate_dir == Path::new(".") {
                0
            } else {
                crate_info.crate_dir.components().count()
            };

            match winner {
                Some((_, current_rank)) if current_rank >= rank => {}
                _ => winner = Some((index, rank)),
            }
        }

        if let Some((index, _)) = winner {
            affected.insert(crates[index].name.clone());
        }
    }

    let mut ordered = affected.into_iter().collect::<Vec<_>>();
    ordered.sort();
    ordered
}

fn crate_contains_file(crate_dir: &Path, file: &Path) -> bool {
    if crate_dir == Path::new(".") {
        return true;
    }
    file.starts_with(crate_dir)
}

fn collect_cargo_manifests(dir: &Path, manifests: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(dir).with_context(|| format!("failed reading {}", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("failed reading entry in {}", dir.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed reading entry type in {}", dir.display()))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let path = entry.path();

        if file_type.is_dir() {
            if name == ".git" || name == "target" {
                continue;
            }
            collect_cargo_manifests(&path, manifests)?;
            continue;
        }

        if file_type.is_file() && name == "Cargo.toml" {
            manifests.push(path);
        }
    }

    Ok(())
}

fn set_manifest_version(manifest_path: &Path, next_version: &Version) -> Result<bool> {
    let original = fs::read_to_string(manifest_path)
        .with_context(|| format!("failed reading {}", manifest_path.display()))?;
    let mut doc = original
        .parse::<DocumentMut>()
        .with_context(|| format!("failed parsing {}", manifest_path.display()))?;

    let mut changed = false;

    if set_item_version(&mut doc, "package", next_version)? {
        changed = true;
    }

    if let Some(workspace) = doc.get_mut("workspace")
        && workspace.is_table_like()
        && let Some(workspace_package) = workspace
            .as_table_like_mut()
            .and_then(|table| table.get_mut("package"))
        && set_table_item_version(workspace_package, next_version)?
    {
        changed = true;
    }

    if !changed {
        return Ok(false);
    }

    let rendered = doc.to_string();
    if rendered == original {
        return Ok(false);
    }

    fs::write(manifest_path, rendered)
        .with_context(|| format!("failed writing {}", manifest_path.display()))?;
    Ok(true)
}

fn set_item_version(
    doc: &mut DocumentMut,
    item_name: &str,
    next_version: &Version,
) -> Result<bool> {
    let Some(item) = doc.get_mut(item_name) else {
        return Ok(false);
    };
    set_table_item_version(item, next_version)
}

fn set_table_item_version(item: &mut Item, next_version: &Version) -> Result<bool> {
    let Some(table) = item.as_table_like_mut() else {
        return Ok(false);
    };

    if !table.contains_key("version") {
        return Ok(false);
    }

    table.insert("version", value(next_version.to_string()));
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use release_kthx_domain::BumpLevel;
    use std::path::Path;
    use std::process::Command;

    fn test_plan(crate_name: &str, current: &str, next: &str) -> CrateReleasePlan {
        CrateReleasePlan {
            crate_name: crate_name.to_string(),
            manifest_path: PathBuf::from(format!("crates/{crate_name}/Cargo.toml")),
            plan: ReleasePlan {
                base_tag: None,
                current_version: Version::parse(current).expect("valid semver"),
                next_version: Version::parse(next).expect("valid semver"),
                bump_level: BumpLevel::Patch,
                commits: Vec::new(),
            },
        }
    }

    fn write_crate(
        root: &Path,
        relative: &str,
        name: &str,
        version: &str,
        private: bool,
        extra: &str,
    ) {
        let manifest_dir = root.join(relative);
        fs::create_dir_all(manifest_dir.join("src")).expect("create crate dirs");

        let publish = if private { "publish = false\n" } else { "" };
        fs::write(
            manifest_dir.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{name}\"\n{publish}version = \"{version}\"\nedition = \"2024\"\n\n{extra}",
            ),
        )
        .expect("write crate manifest");
    }

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

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        run_git_ok(dir.path(), &["init"]);
        run_git_ok(dir.path(), &["config", "user.name", "tester"]);
        run_git_ok(dir.path(), &["config", "user.email", "tester@example.com"]);
        dir
    }

    fn commit_repo_files(repo: &Path, files: &[(&str, &str)], subject: &str, body: Option<&str>) {
        for (relative, content) in files {
            let path = repo.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent directories");
            }
            fs::write(&path, content).expect("write file");
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
    }

    #[test]
    fn tag_template_requires_crate_placeholder_for_multiple_crates() {
        let result = render_tag_name(
            "v{{ version }}",
            "my-crate",
            &Version::parse("0.2.0").expect("valid semver"),
            2,
        );
        assert!(result.is_err());
    }

    #[test]
    fn tag_template_renders_crate_and_version() {
        let tag = render_tag_name(
            "{{ crate }}-v{{ version }}",
            "my-crate",
            &Version::parse("0.2.0").expect("valid semver"),
            2,
        )
        .expect("tag renders");
        assert_eq!(tag, "my-crate-v0.2.0");
    }

    #[test]
    fn updates_workspace_package_versions_in_lockfile() {
        let temp = tempfile::tempdir().expect("temp dir");
        let lock_path = temp.path().join("Cargo.lock");
        fs::write(
            &lock_path,
            "version = 4\n\n[[package]]\nname = \"release-kthx\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"release-kthx-domain\"\nversion = \"0.1.0\"\n",
        )
        .expect("write lock");

        let plans = vec![
            test_plan("release-kthx", "0.1.0", "0.1.1"),
            test_plan("release-kthx-domain", "0.1.0", "0.2.0"),
        ];

        let changed = set_lockfile_versions(temp.path(), &plans).expect("lock update");
        assert!(changed);

        let updated = fs::read_to_string(&lock_path).expect("read updated lock");
        assert!(updated.contains("name = \"release-kthx\"\nversion = \"0.1.1\""));
        assert!(updated.contains("name = \"release-kthx-domain\"\nversion = \"0.2.0\""));
    }

    #[test]
    fn release_payload_accepts_manifest_and_lockfile() {
        let files = vec![
            PathBuf::from("Cargo.toml"),
            PathBuf::from("Cargo.lock"),
            PathBuf::from("crates/release-kthx-domain/Cargo.toml"),
        ];
        assert!(is_release_merge_payload(&files));
    }

    #[test]
    fn release_payload_rejects_non_release_files() {
        let files = vec![PathBuf::from("Cargo.toml"), PathBuf::from("README.md")];
        assert!(!is_release_merge_payload(&files));
    }

    #[test]
    fn build_crate_release_notes_uses_previous_tag_boundary() {
        let repo = init_repo();
        let repo_path = repo.path();

        commit_repo_files(
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
        run_git_ok(repo_path, &["tag", "demo-v0.1.0"]);

        commit_repo_files(
            repo_path,
            &[("src/lib.rs", "pub fn one() {}\npub fn two() {}\n")],
            "feat: add api",
            None,
        );

        commit_repo_files(
            repo_path,
            &[(
                "Cargo.toml",
                "[package]\nname = \"demo\"\nversion = \"0.2.0\"\nedition = \"2024\"\n",
            )],
            "chore(release): demo v0.2.0",
            None,
        );

        let notes = build_crate_release_notes(repo_path, "{{ crate }}-v{{ version }}")
            .expect("build release notes");
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].crate_name, "demo");
        assert_eq!(notes[0].base_ref.as_deref(), Some("demo-v0.1.0"));
        assert_eq!(notes[0].commits.len(), 1);
        assert_eq!(notes[0].commits[0].subject, "feat: add api");
    }

    #[test]
    fn collect_crates_populates_local_dependencies() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path();

        fs::create_dir_all(root.join("crates/app/src")).expect("create app dirs");
        fs::create_dir_all(root.join("crates/domain/src")).expect("create domain dirs");

        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/app\", \"crates/domain\"]\n",
        )
        .expect("write workspace manifest");

        fs::write(
            root.join("crates/domain/Cargo.toml"),
            "[package]\nname = \"domain\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n",
        )
        .expect("write domain manifest");

        fs::write(
            root.join("crates/app/Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\ndomain = { path = \"../domain\", version = \"0.1.0\" }\nserde = \"1\"\n",
        )
        .expect("write app manifest");

        let crates = collect_crates(root).expect("collect crates");

        let app = crates
            .iter()
            .find(|crate_info| crate_info.name == "app")
            .expect("app crate");
        assert_eq!(app.local_dependencies, vec!["domain"]);

        let domain = crates
            .iter()
            .find(|crate_info| crate_info.name == "domain")
            .expect("domain crate");
        assert!(domain.local_dependencies.is_empty());
    }

    #[test]
    fn auto_policy_strips_private_internal_dependency_versions() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path();

        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/app\", \"crates/domain\"]\n",
        )
        .expect("write workspace manifest");
        write_crate(root, "crates/domain", "domain", "0.4.0", true, "");
        write_crate(
            root,
            "crates/app",
            "app",
            "0.1.0",
            true,
            "[dependencies]\ndomain = { path = \"../domain\", version = \"0.4.0\", features = [\"serde\"] }\n",
        );

        let drifts = internal_dependency_drifts(root, InternalDependencyPolicy::Auto)
            .expect("detect drifts");
        assert_eq!(drifts.len(), 1);
        assert_eq!(drifts[0].new_requirement, None);

        let changed = set_internal_dependency_requirements(
            root,
            InternalDependencyPolicy::Auto,
            &[test_plan("domain", "0.4.0", "0.5.0")],
        )
        .expect("rewrite internal deps");
        assert_eq!(changed, vec![PathBuf::from("crates/app/Cargo.toml")]);

        let updated = fs::read_to_string(root.join("crates/app/Cargo.toml")).expect("read app");
        assert!(updated.contains("path = \"../domain\""));
        assert!(updated.contains("features = [\"serde\"]"));
        assert!(!updated.contains("version = \"0.4.0\""));
        assert!(!updated.contains("version = \"0.5.0\""));
    }

    #[test]
    fn auto_policy_updates_non_private_internal_dependency_versions() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path();

        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/app\", \"crates/domain\"]\n",
        )
        .expect("write workspace manifest");
        write_crate(root, "crates/domain", "domain", "0.4.0", false, "");
        write_crate(
            root,
            "crates/app",
            "app",
            "0.1.0",
            true,
            "[dependencies]\ndomain = { path = \"../domain\", version = \"^0.4.0\" }\n",
        );

        let changed = set_internal_dependency_requirements(
            root,
            InternalDependencyPolicy::Auto,
            &[test_plan("domain", "0.4.0", "0.5.0")],
        )
        .expect("rewrite internal deps");
        assert_eq!(changed, vec![PathBuf::from("crates/app/Cargo.toml")]);

        let updated = fs::read_to_string(root.join("crates/app/Cargo.toml")).expect("read app");
        assert!(updated.contains("version = \"^0.5.0\""));
    }

    #[test]
    fn auto_policy_rewrites_workspace_dependency_versions() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path();

        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/app\", \"crates/domain\"]\n\n[workspace.dependencies]\ndomain = { path = \"crates/domain\", version = \"0.4.0\" }\n",
        )
        .expect("write workspace manifest");
        write_crate(root, "crates/domain", "domain", "0.4.0", true, "");
        write_crate(
            root,
            "crates/app",
            "app",
            "0.1.0",
            true,
            "[dependencies]\ndomain = { workspace = true }\n",
        );

        let changed = set_internal_dependency_requirements(
            root,
            InternalDependencyPolicy::Auto,
            &[test_plan("domain", "0.4.0", "0.5.0")],
        )
        .expect("rewrite workspace dependencies");
        assert_eq!(changed, vec![PathBuf::from("Cargo.toml")]);

        let updated = fs::read_to_string(root.join("Cargo.toml")).expect("read root manifest");
        assert!(!updated.contains("version = \"0.4.0\""));
    }

    #[test]
    fn auto_policy_preserves_missing_version_fields_for_non_private_edges() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path();

        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/app\", \"crates/domain\"]\n",
        )
        .expect("write workspace manifest");
        write_crate(root, "crates/domain", "domain", "0.4.0", false, "");
        write_crate(
            root,
            "crates/app",
            "app",
            "0.1.0",
            false,
            "[dependencies]\ndomain = { path = \"../domain\" }\n",
        );

        let drifts = internal_dependency_drifts(root, InternalDependencyPolicy::Auto)
            .expect("detect drifts");
        assert!(drifts.is_empty());
    }

    #[test]
    fn strip_policy_removes_versions_for_publishable_internal_edges() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path();

        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/app\", \"crates/domain\"]\n",
        )
        .expect("write workspace manifest");
        write_crate(root, "crates/domain", "domain", "0.4.0", false, "");
        write_crate(
            root,
            "crates/app",
            "app",
            "0.1.0",
            false,
            "[dependencies]\ndomain = { path = \"../domain\", version = \"^0.4.0\" }\n",
        );

        let changed =
            set_internal_dependency_requirements(root, InternalDependencyPolicy::Strip, &[])
                .expect("rewrite internal deps");
        assert_eq!(changed, vec![PathBuf::from("crates/app/Cargo.toml")]);

        let updated = fs::read_to_string(root.join("crates/app/Cargo.toml")).expect("read app");
        assert!(updated.contains("path = \"../domain\""));
        assert!(!updated.contains("^0.4.0"));
    }

    #[test]
    fn update_policy_inserts_missing_versions_for_internal_edges() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path();

        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/app\", \"crates/domain\"]\n",
        )
        .expect("write workspace manifest");
        write_crate(root, "crates/domain", "domain", "0.4.0", false, "");
        write_crate(
            root,
            "crates/app",
            "app",
            "0.1.0",
            false,
            "[dependencies]\ndomain = { path = \"../domain\" }\n",
        );

        let changed = set_internal_dependency_requirements(
            root,
            InternalDependencyPolicy::Update,
            &[test_plan("domain", "0.4.0", "0.5.0")],
        )
        .expect("rewrite internal deps");
        assert_eq!(changed, vec![PathBuf::from("crates/app/Cargo.toml")]);

        let updated = fs::read_to_string(root.join("crates/app/Cargo.toml")).expect("read app");
        assert!(updated.contains("path = \"../domain\""));
        assert!(updated.contains("version = \"0.5.0\""));
    }

    #[test]
    fn update_policy_rewrites_renamed_and_target_specific_dependency_sections() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path();

        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/app\", \"crates/domain\", \"crates/helper\"]\n",
        )
        .expect("write workspace manifest");
        write_crate(root, "crates/domain", "domain", "0.4.0", false, "");
        write_crate(root, "crates/helper", "helper", "0.2.0", false, "");
        write_crate(
            root,
            "crates/app",
            "app",
            "0.1.0",
            false,
            r#"[dependencies]
domain-api = { package = "domain", path = "../domain", version = "^0.4.0" }

[dev-dependencies]
helper = { path = "../helper", version = "0.2.0" }

[build-dependencies]
helper-build = { package = "helper", path = "../helper", version = "0.2.0" }

[target.'cfg(unix)'.dependencies]
domain-unix = { package = "domain", path = "../domain", version = "^0.4.0" }

[target.'cfg(unix)'.dev-dependencies]
helper = { path = "../helper", version = "0.2.0" }

[target.'cfg(unix)'.build-dependencies]
helper-build = { package = "helper", path = "../helper", version = "0.2.0" }
"#,
        );

        let changed = set_internal_dependency_requirements(
            root,
            InternalDependencyPolicy::Update,
            &[
                test_plan("domain", "0.4.0", "0.5.0"),
                test_plan("helper", "0.2.0", "0.3.0"),
            ],
        )
        .expect("rewrite internal deps");
        assert_eq!(changed, vec![PathBuf::from("crates/app/Cargo.toml")]);

        let updated = fs::read_to_string(root.join("crates/app/Cargo.toml")).expect("read app");
        assert_eq!(updated.matches("version = \"^0.5.0\"").count(), 2);
        assert_eq!(updated.matches("version = \"0.3.0\"").count(), 4);
        assert!(updated.contains(
            "domain-api = { package = \"domain\", path = \"../domain\", version = \"^0.5.0\" }"
        ));
        assert!(updated.contains(
            "domain-unix = { package = \"domain\", path = \"../domain\", version = \"^0.5.0\" }"
        ));
        assert!(updated.contains(
            "helper-build = { package = \"helper\", path = \"../helper\", version = \"0.3.0\" }"
        ));
    }

    #[test]
    fn update_policy_rewrites_workspace_dependency_versions() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path();

        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/app\", \"crates/domain\"]\n\n[workspace.dependencies]\ndomain = { path = \"crates/domain\", version = \"^0.4.0\" }\n",
        )
        .expect("write workspace manifest");
        write_crate(root, "crates/domain", "domain", "0.4.0", false, "");
        write_crate(
            root,
            "crates/app",
            "app",
            "0.1.0",
            false,
            "[dependencies]\ndomain = { workspace = true }\n",
        );

        let changed = set_internal_dependency_requirements(
            root,
            InternalDependencyPolicy::Update,
            &[test_plan("domain", "0.4.0", "0.5.0")],
        )
        .expect("rewrite workspace dependencies");
        assert_eq!(changed, vec![PathBuf::from("Cargo.toml")]);

        let updated = fs::read_to_string(root.join("Cargo.toml")).expect("read root manifest");
        assert!(updated.contains("domain = { path = \"crates/domain\", version = \"^0.5.0\" }"));

        let app_manifest =
            fs::read_to_string(root.join("crates/app/Cargo.toml")).expect("read app manifest");
        assert!(app_manifest.contains("domain = { workspace = true }"));
        assert!(!app_manifest.contains("0.5.0"));
    }

    #[test]
    fn unsupported_internal_dependency_requirements_fail_rewrite() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path();

        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/app\", \"crates/domain\"]\n",
        )
        .expect("write workspace manifest");
        write_crate(root, "crates/domain", "domain", "0.4.0", false, "");
        write_crate(
            root,
            "crates/app",
            "app",
            "0.1.0",
            false,
            "[dependencies]\ndomain = { path = \"../domain\", version = \">=0.4.0, <0.5.0\" }\n",
        );

        let error = set_internal_dependency_requirements(
            root,
            InternalDependencyPolicy::Update,
            &[test_plan("domain", "0.4.0", "0.5.0")],
        )
        .expect_err("rewrite should fail");
        let rendered = format!("{error:#}");
        assert!(rendered.contains(
            "failed parsing internal dependency requirement for domain in crates/app/Cargo.toml"
        ));
        assert!(
            rendered
                .contains("unsupported internal dependency version requirement `>=0.4.0, <0.5.0`")
        );
    }
}
