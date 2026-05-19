use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use brehon_types::task::normalize_task_status;
use git2::{Oid, Repository, Sort};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
struct TaskRecord {
    #[serde(rename = "task_id", alias = "id")]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    status: String,
    #[serde(default = "default_task_type")]
    task_type: String,
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    integration_status: Option<String>,
    #[serde(default)]
    merge_target: Option<String>,
    #[serde(default)]
    latest_commit: Option<String>,
    #[serde(default)]
    merged_commit: Option<String>,
    #[serde(default)]
    integration_branch: Option<String>,
    #[serde(default)]
    integration_worktree: Option<String>,
}

fn default_task_type() -> String {
    "task".to_string()
}

#[derive(Debug, Serialize)]
pub struct EpicTruthfulnessReport {
    pub epic_id: String,
    pub epic_status: String,
    pub integration_branch: Option<String>,
    pub integration_worktree: Option<String>,
    pub default_branch: String,
    pub default_branch_advancement: DefaultBranchAdvancement,
    pub subtasks: Vec<SubtaskTruthStatus>,
}

#[derive(Debug, Serialize)]
pub struct DefaultBranchAdvancement {
    pub advanced: bool,
    pub commit_count: usize,
    pub commits: Vec<String>,
    pub related_subtask_ids: Vec<String>,
    pub summary: String,
}

#[derive(Debug, Serialize)]
pub struct SubtaskTruthStatus {
    pub task_id: String,
    pub title: String,
    pub lifecycle_status: String,
    pub truth_status: String,
    pub integration_status: Option<String>,
    pub merge_target: Option<String>,
    pub subtask_head: Option<String>,
    pub epic_head: Option<String>,
    pub head_relation: Option<HeadRelation>,
}

#[derive(Debug, Serialize)]
pub struct HeadRelation {
    pub state: String,
    pub ahead: usize,
    pub behind: usize,
}

pub fn execute(epic_id: &str, project_path: Option<&Path>) -> Result<()> {
    let report = build_report(epic_id, project_path)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&report)
            .context("failed to serialize epic truthfulness report")?
    );
    Ok(())
}

pub(crate) fn build_report(
    epic_id: &str,
    project_path: Option<&Path>,
) -> Result<EpicTruthfulnessReport> {
    let project_root = resolve_project_root(project_path)?;
    let tasks = load_tasks(&project_root)?;

    let epic = tasks
        .iter()
        .find(|task| task.id == epic_id)
        .ok_or_else(|| anyhow!("Epic task not found: {epic_id}"))?;

    if epic.task_type != "epic" {
        return Err(anyhow!(
            "Task {epic_id} is not an epic (task_type={})",
            epic.task_type
        ));
    }

    let repo = Repository::discover(&project_root).with_context(|| {
        format!(
            "failed to open git repository from {}",
            project_root.display()
        )
    })?;
    let default_branch = detect_default_branch(&repo);

    let integration_branch = epic
        .integration_branch
        .as_ref()
        .map(|branch| branch.trim().to_string())
        .filter(|branch| !branch.is_empty());
    let integration_worktree = epic
        .integration_worktree
        .as_ref()
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty());

    let epic_head_oid = integration_branch
        .as_deref()
        .and_then(|branch| resolve_branch_head(&repo, branch));
    let epic_head = epic_head_oid.map(|oid| oid.to_string());

    let default_advancement = build_default_branch_advancement(
        &repo,
        &default_branch,
        epic_head_oid,
        tasks
            .iter()
            .filter(|task| task.parent_id.as_deref() == Some(epic_id)),
    );

    let mut subtasks: Vec<SubtaskTruthStatus> = tasks
        .iter()
        .filter(|task| task.parent_id.as_deref() == Some(epic_id))
        .map(|task| {
            let subtask_head = task
                .merged_commit
                .as_ref()
                .filter(|commit| !commit.trim().is_empty())
                .cloned()
                .or_else(|| {
                    task.latest_commit
                        .as_ref()
                        .filter(|commit| !commit.trim().is_empty())
                        .cloned()
                });

            let head_relation = subtask_head
                .as_deref()
                .and_then(|commit| Oid::from_str(commit).ok())
                .zip(epic_head_oid)
                .and_then(|(subtask_head_oid, epic_oid)| {
                    head_relation(&repo, subtask_head_oid, epic_oid)
                });

            SubtaskTruthStatus {
                task_id: task.id.clone(),
                title: task.title.clone(),
                lifecycle_status: normalized_or_raw_status(&task.status),
                truth_status: classify_truth_status(task, &default_branch),
                integration_status: task.integration_status.clone(),
                merge_target: task.merge_target.clone(),
                subtask_head,
                epic_head: epic_head.clone(),
                head_relation,
            }
        })
        .collect();

    subtasks.sort_by(|left, right| left.task_id.cmp(&right.task_id));

    Ok(EpicTruthfulnessReport {
        epic_id: epic_id.to_string(),
        epic_status: normalized_or_raw_status(&epic.status),
        integration_branch,
        integration_worktree,
        default_branch,
        default_branch_advancement: default_advancement,
        subtasks,
    })
}

fn classify_truth_status(task: &TaskRecord, default_branch: &str) -> String {
    let normalized = normalize_task_status(&task.status).unwrap_or("pending");
    let merge_target = task
        .merge_target
        .as_deref()
        .filter(|target| !target.is_empty());
    let is_default_target = merge_target.is_some_and(|target| target == default_branch);
    let is_epic_target = merge_target.is_some_and(|target| target != default_branch);
    let is_integrated = task
        .integration_status
        .as_deref()
        .is_some_and(|status| status == "integrated");

    if normalized == "merged" && is_default_target {
        return "merged_to_main".to_string();
    }

    if is_integrated && is_epic_target {
        return "integrated_into_epic".to_string();
    }

    if normalized == "approved" {
        return "approved_not_integrated".to_string();
    }

    if normalized == "in_review" {
        return "in_review".to_string();
    }

    if normalized == "blocked" {
        return "blocked".to_string();
    }

    if normalized == "changes_requested" {
        return "changes_requested".to_string();
    }

    if matches!(normalized, "assigned" | "in_progress") {
        return "in_progress".to_string();
    }

    "pending".to_string()
}

fn build_default_branch_advancement<'a>(
    repo: &Repository,
    default_branch: &str,
    epic_head_oid: Option<Oid>,
    subtasks: impl Iterator<Item = &'a TaskRecord>,
) -> DefaultBranchAdvancement {
    let Some(default_oid) = resolve_branch_head(repo, default_branch) else {
        return DefaultBranchAdvancement {
            advanced: false,
            commit_count: 0,
            commits: Vec::new(),
            related_subtask_ids: Vec::new(),
            summary: format!("Default branch '{default_branch}' could not be resolved in git."),
        };
    };

    let Some(epic_oid) = epic_head_oid else {
        return DefaultBranchAdvancement {
            advanced: false,
            commit_count: 0,
            commits: Vec::new(),
            related_subtask_ids: Vec::new(),
            summary: "Epic integration branch HEAD is unavailable; cannot compare default-branch movement."
                .to_string(),
        };
    };

    let commits = revwalk_commits_not_in(repo, default_oid, epic_oid).unwrap_or_default();
    let commit_set: HashSet<&str> = commits.iter().map(String::as_str).collect();

    let mut related_subtask_ids: Vec<String> = subtasks
        .filter_map(|task| {
            let hits_latest = task
                .latest_commit
                .as_deref()
                .is_some_and(|commit| commit_set.contains(commit));
            let hits_merged = task
                .merged_commit
                .as_deref()
                .is_some_and(|commit| commit_set.contains(commit));
            (hits_latest || hits_merged).then(|| task.id.clone())
        })
        .collect();
    related_subtask_ids.sort();
    related_subtask_ids.dedup();

    let advanced = !commits.is_empty();
    let summary = if advanced {
        format!(
            "Default branch '{default_branch}' advanced by {} commit(s) relative to the epic branch.",
            commits.len()
        )
    } else {
        format!("Default branch '{default_branch}' has not advanced relative to the epic branch.")
    };

    DefaultBranchAdvancement {
        advanced,
        commit_count: commits.len(),
        commits,
        related_subtask_ids,
        summary,
    }
}

fn head_relation(repo: &Repository, subtask_head: Oid, epic_head: Oid) -> Option<HeadRelation> {
    let ahead = count_commits_not_in(repo, subtask_head, epic_head).ok()?;
    let behind = count_commits_not_in(repo, epic_head, subtask_head).ok()?;
    let state = match (ahead, behind) {
        (0, 0) => "clean",
        (a, 0) if a > 0 => "ahead",
        (0, b) if b > 0 => "behind",
        _ => "diverged",
    }
    .to_string();

    Some(HeadRelation {
        state,
        ahead,
        behind,
    })
}

fn count_commits_not_in(repo: &Repository, source: Oid, hide: Oid) -> Result<usize> {
    let mut revwalk = repo.revwalk().context("failed to create git revwalk")?;
    revwalk
        .push(source)
        .with_context(|| format!("failed to push source commit {source}"))?;
    revwalk
        .hide(hide)
        .with_context(|| format!("failed to hide commit {hide}"))?;
    Ok(revwalk.filter_map(std::result::Result::ok).count())
}

fn revwalk_commits_not_in(repo: &Repository, source: Oid, hide: Oid) -> Result<Vec<String>> {
    let mut revwalk = repo.revwalk().context("failed to create git revwalk")?;
    let _ = revwalk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME);
    revwalk
        .push(source)
        .with_context(|| format!("failed to push source commit {source}"))?;
    revwalk
        .hide(hide)
        .with_context(|| format!("failed to hide commit {hide}"))?;

    let commits: Vec<String> = revwalk
        .filter_map(std::result::Result::ok)
        .map(|oid| oid.to_string())
        .collect();
    Ok(commits)
}

fn normalized_or_raw_status(status: &str) -> String {
    normalize_task_status(status).unwrap_or(status).to_string()
}

fn resolve_project_root(project_path: Option<&Path>) -> Result<PathBuf> {
    let path = match project_path {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().context("failed to resolve current working directory")?,
    };

    if path.file_name().is_some_and(|name| name == ".brehon") {
        path.parent()
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("invalid BREHON_ROOT path: {}", path.display()))
    } else {
        Ok(path)
    }
}

fn tasks_dir(project_root: &Path) -> PathBuf {
    project_root.join(".brehon").join("runtime").join("tasks")
}

fn load_tasks(project_root: &Path) -> Result<Vec<TaskRecord>> {
    let dir = tasks_dir(project_root);
    let entries = fs::read_dir(&dir)
        .with_context(|| format!("failed to read tasks directory {}", dir.display()))?;

    let mut tasks = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read task file {}", path.display()))?;
        let task: TaskRecord = serde_json::from_str(&content)
            .with_context(|| format!("failed to parse task file {}", path.display()))?;
        tasks.push(task);
    }

    Ok(tasks)
}

fn detect_default_branch(repo: &Repository) -> String {
    if let Ok(origin_head) = repo.find_reference("refs/remotes/origin/HEAD") {
        if let Some(target) = origin_head.symbolic_target() {
            if let Some(stripped) = target.strip_prefix("refs/remotes/origin/") {
                return stripped.to_string();
            }
        }
    }

    for candidate in ["main", "master", "develop"] {
        if resolve_branch_head(repo, candidate).is_some() {
            return candidate.to_string();
        }
    }

    repo.head()
        .ok()
        .and_then(|head| head.shorthand().map(str::to_string))
        .unwrap_or_else(|| "main".to_string())
}

fn resolve_branch_head(repo: &Repository, branch: &str) -> Option<Oid> {
    let local_ref = if branch.starts_with("refs/") {
        branch.to_string()
    } else {
        format!("refs/heads/{branch}")
    };

    repo.find_reference(&local_ref)
        .ok()
        .and_then(|reference| reference.peel_to_commit().ok())
        .map(|commit| commit.id())
        .or_else(|| {
            let remote_ref = format!("refs/remotes/origin/{branch}");
            repo.find_reference(&remote_ref)
                .ok()
                .and_then(|reference| reference.peel_to_commit().ok())
                .map(|commit| commit.id())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use git2::build::CheckoutBuilder;
    use git2::Signature;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn approved_subtask_is_reported_as_approved_not_integrated() {
        let fixture = TestFixture::new().expect("fixture");

        let main_commit = fixture.commit("seed", "seed.txt", "seed").expect("seed");
        fixture
            .create_and_checkout_branch("epic/feature", &main_commit)
            .expect("epic branch");
        let _epic_head = fixture
            .commit("epic head", "epic.txt", "epic")
            .expect("epic head");

        fixture
            .create_and_checkout_branch("worker/subtask-1", &main_commit)
            .expect("worker branch");
        let worker_head = fixture
            .commit("subtask work", "subtask.txt", "wip")
            .expect("worker head");

        fixture
            .write_task(json!({
                "id": "EPIC-1",
                "title": "Feature epic",
                "status": "InProgress",
                "task_type": "epic",
                "integration_branch": "epic/feature",
                "integration_worktree": "/tmp/epic-feature"
            }))
            .expect("write epic task");
        fixture
            .write_task(json!({
                "id": "T-1",
                "title": "Subtask",
                "status": "Approved",
                "task_type": "task",
                "parent_id": "EPIC-1",
                "merge_target": "epic/feature",
                "integration_status": "pending",
                "latest_commit": worker_head,
            }))
            .expect("write subtask");

        let report = build_report("EPIC-1", Some(fixture.root())).expect("report");
        assert_eq!(report.subtasks.len(), 1);
        assert_eq!(report.subtasks[0].truth_status, "approved_not_integrated");
    }

    #[test]
    fn task_store_shape_with_task_id_is_supported() {
        let fixture = TestFixture::new().expect("fixture");
        let main_commit = fixture.commit("seed", "seed.txt", "seed").expect("seed");
        fixture
            .create_and_checkout_branch("epic/feature", &main_commit)
            .expect("epic branch");

        fixture
            .write_task(json!({
                "task_id": "EPIC-1",
                "title": "Feature epic",
                "status": "InProgress",
                "task_type": "epic",
                "integration_branch": "epic/feature",
                "integration_worktree": "/tmp/epic-feature"
            }))
            .expect("write epic task");

        let report = build_report("EPIC-1", Some(fixture.root())).expect("report");
        assert_eq!(report.epic_id, "EPIC-1");
    }

    #[test]
    fn integrated_subtask_is_reported_as_integrated_into_epic() {
        let fixture = TestFixture::new().expect("fixture");
        let main_commit = fixture.commit("seed", "seed.txt", "seed").expect("seed");

        fixture
            .create_and_checkout_branch("epic/feature", &main_commit)
            .expect("epic branch");
        let epic_head = fixture
            .commit("integrated", "integrated.txt", "done")
            .expect("integrated commit");

        fixture
            .write_task(json!({
                "id": "EPIC-1",
                "title": "Feature epic",
                "status": "InProgress",
                "task_type": "epic",
                "integration_branch": "epic/feature",
                "integration_worktree": "/tmp/epic-feature"
            }))
            .expect("write epic task");
        fixture
            .write_task(json!({
                "id": "T-2",
                "title": "Integrated subtask",
                "status": "closed",
                "task_type": "task",
                "parent_id": "EPIC-1",
                "merge_target": "epic/feature",
                "integration_status": "integrated",
                "merged_commit": epic_head,
            }))
            .expect("write subtask");

        let report = build_report("EPIC-1", Some(fixture.root())).expect("report");
        assert_eq!(report.subtasks[0].truth_status, "integrated_into_epic");

        let relation = report.subtasks[0]
            .head_relation
            .as_ref()
            .expect("head relation");
        assert_eq!(relation.state, "clean");
        assert_eq!(relation.ahead, 0);
        assert_eq!(relation.behind, 0);
    }

    #[test]
    fn mixed_epic_statuses_and_default_not_advanced_are_reported() {
        let fixture = TestFixture::new().expect("fixture");
        let main_commit = fixture.commit("seed", "seed.txt", "seed").expect("seed");

        fixture
            .create_and_checkout_branch("epic/feature", &main_commit)
            .expect("epic branch");
        let integrated_commit = fixture
            .commit("integrated", "integrated.txt", "done")
            .expect("integrated commit");

        fixture
            .create_and_checkout_branch("worker/review", &main_commit)
            .expect("worker branch");
        let review_commit = fixture
            .commit("review", "review.txt", "pending")
            .expect("review commit");

        fixture
            .write_task(json!({
                "id": "EPIC-1",
                "title": "Feature epic",
                "status": "InProgress",
                "task_type": "epic",
                "integration_branch": "epic/feature",
                "integration_worktree": "/tmp/epic-feature"
            }))
            .expect("write epic task");
        fixture
            .write_task(json!({
                "id": "T-1",
                "title": "Integrated subtask",
                "status": "closed",
                "task_type": "task",
                "parent_id": "EPIC-1",
                "merge_target": "epic/feature",
                "integration_status": "integrated",
                "merged_commit": integrated_commit,
            }))
            .expect("write integrated subtask");
        fixture
            .write_task(json!({
                "id": "T-2",
                "title": "In review subtask",
                "status": "InReview",
                "task_type": "task",
                "parent_id": "EPIC-1",
                "merge_target": "epic/feature",
                "integration_status": "pending",
                "latest_commit": review_commit,
            }))
            .expect("write review subtask");

        let report = build_report("EPIC-1", Some(fixture.root())).expect("report");

        assert_eq!(report.default_branch_advancement.advanced, false);
        assert_eq!(report.default_branch_advancement.commit_count, 0);
        assert!(report
            .default_branch_advancement
            .summary
            .contains("has not advanced"));

        let integrated = report
            .subtasks
            .iter()
            .find(|task| task.task_id == "T-1")
            .expect("integrated task");
        assert_eq!(integrated.truth_status, "integrated_into_epic");

        let in_review = report
            .subtasks
            .iter()
            .find(|task| task.task_id == "T-2")
            .expect("review task");
        assert_eq!(in_review.truth_status, "in_review");
    }

    #[test]
    fn merged_to_main_is_reported_even_when_epic_is_open() {
        let fixture = TestFixture::new().expect("fixture");
        let main_commit = fixture.commit("seed", "seed.txt", "seed").expect("seed");

        fixture
            .create_and_checkout_branch("epic/feature", &main_commit)
            .expect("epic branch");
        let _epic_head = fixture
            .commit("epic work", "epic.txt", "epic")
            .expect("epic head");

        fixture
            .create_and_checkout_branch("main", &main_commit)
            .expect("checkout main");
        let merged_to_main = fixture
            .commit("merged to main", "main-only.txt", "main")
            .expect("main commit");

        fixture
            .write_task(json!({
                "task_id": "EPIC-1",
                "title": "Open Feature epic",
                "status": "InProgress",
                "task_type": "epic",
                "integration_branch": "epic/feature",
                "integration_worktree": "/tmp/epic-feature"
            }))
            .expect("write epic task");
        fixture
            .write_task(json!({
                "task_id": "T-merged-main",
                "title": "Merged to main",
                "status": "Merged",
                "task_type": "task",
                "parent_id": "EPIC-1",
                "merge_target": "main",
                "merged_commit": merged_to_main
            }))
            .expect("write subtask");

        let report = build_report("EPIC-1", Some(fixture.root())).expect("report");
        assert_eq!(report.subtasks.len(), 1);
        assert_eq!(report.subtasks[0].truth_status, "merged_to_main");
    }

    #[test]
    fn closed_on_epic_branch_without_integrated_flag_is_not_reported_integrated() {
        let fixture = TestFixture::new().expect("fixture");
        let main_commit = fixture.commit("seed", "seed.txt", "seed").expect("seed");

        fixture
            .create_and_checkout_branch("epic/feature", &main_commit)
            .expect("epic branch");
        let _epic_head = fixture
            .commit("epic work", "epic.txt", "epic")
            .expect("epic head");

        fixture
            .write_task(json!({
                "task_id": "EPIC-1",
                "title": "Feature epic",
                "status": "InProgress",
                "task_type": "epic",
                "integration_branch": "epic/feature",
                "integration_worktree": "/tmp/epic-feature"
            }))
            .expect("write epic task");
        fixture
            .write_task(json!({
                "task_id": "T-closed",
                "title": "Closed without integration",
                "status": "closed",
                "task_type": "task",
                "parent_id": "EPIC-1",
                "merge_target": "epic/feature",
                "integration_status": "pending"
            }))
            .expect("write subtask");

        let report = build_report("EPIC-1", Some(fixture.root())).expect("report");
        assert_eq!(report.subtasks[0].truth_status, "pending");
    }

    #[test]
    fn default_branch_commit_order_is_chronological_not_lexicographic() {
        let fixture = TestFixture::new().expect("fixture");
        let main_base = fixture.commit("seed", "seed.txt", "seed").expect("seed");

        fixture
            .create_and_checkout_branch("epic/feature", &main_base)
            .expect("epic branch");
        let _epic_head = fixture
            .commit("epic head", "epic.txt", "epic")
            .expect("epic head");

        fixture
            .create_and_checkout_branch("main", &main_base)
            .expect("checkout main");
        let older_main = fixture
            .commit("main older", "m1.txt", "one")
            .expect("older main");
        let newer_main = fixture
            .commit("main newer", "m2.txt", "two")
            .expect("newer main");

        fixture
            .write_task(json!({
                "task_id": "EPIC-1",
                "title": "Feature epic",
                "status": "InProgress",
                "task_type": "epic",
                "integration_branch": "epic/feature",
                "integration_worktree": "/tmp/epic-feature"
            }))
            .expect("write epic task");

        let report = build_report("EPIC-1", Some(fixture.root())).expect("report");
        assert_eq!(report.default_branch_advancement.commits.len(), 2);
        assert_eq!(report.default_branch_advancement.commits[0], newer_main);
        assert_eq!(report.default_branch_advancement.commits[1], older_main);
    }

    struct TestFixture {
        temp_dir: TempDir,
        repo: Repository,
    }

    impl TestFixture {
        fn new() -> Result<Self> {
            let temp_dir = TempDir::new().context("tempdir")?;
            let repo = Repository::init(temp_dir.path()).context("init repository")?;

            let fixture = Self { temp_dir, repo };
            fixture.init_main_branch()?;
            fixture.ensure_tasks_dir()?;
            Ok(fixture)
        }

        fn root(&self) -> &Path {
            self.temp_dir.path()
        }

        fn ensure_tasks_dir(&self) -> Result<()> {
            fs::create_dir_all(tasks_dir(self.root())).context("create tasks dir")?;
            Ok(())
        }

        fn write_task(&self, value: serde_json::Value) -> Result<()> {
            let id = value
                .get("task_id")
                .or_else(|| value.get("id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("task missing task_id/id"))?;
            let path = tasks_dir(self.root()).join(format!("{id}.json"));
            fs::write(path, serde_json::to_vec_pretty(&value)?).context("write task file")?;
            Ok(())
        }

        fn init_main_branch(&self) -> Result<()> {
            let oid = self.create_commit_internal("initial", "README.md", "init", None)?;
            self.repo
                .reference("refs/heads/main", oid, true, "create main")
                .context("create main ref")?;
            self.repo
                .set_head("refs/heads/main")
                .context("set head main")?;
            self.repo
                .checkout_head(Some(CheckoutBuilder::new().force()))
                .context("checkout main")?;
            Ok(())
        }

        fn commit(&self, message: &str, file: &str, content: &str) -> Result<String> {
            let parent = self
                .repo
                .head()
                .ok()
                .and_then(|head| head.target())
                .and_then(|oid| self.repo.find_commit(oid).ok());
            let oid = self.create_commit_internal(message, file, content, parent.as_ref())?;
            Ok(oid.to_string())
        }

        fn create_and_checkout_branch(&self, branch: &str, base_commit: &str) -> Result<()> {
            let base_oid = Oid::from_str(base_commit).context("parse base commit")?;
            let base = self
                .repo
                .find_commit(base_oid)
                .with_context(|| format!("find base commit {base_commit}"))?;
            self.repo
                .branch(branch, &base, true)
                .with_context(|| format!("create branch {branch}"))?;
            self.repo
                .set_head(&format!("refs/heads/{branch}"))
                .with_context(|| format!("set head to {branch}"))?;
            self.repo
                .checkout_head(Some(CheckoutBuilder::new().force()))
                .with_context(|| format!("checkout {branch}"))?;
            Ok(())
        }

        fn create_commit_internal(
            &self,
            message: &str,
            file: &str,
            content: &str,
            parent: Option<&git2::Commit<'_>>,
        ) -> Result<Oid> {
            let workdir = self
                .repo
                .workdir()
                .ok_or_else(|| anyhow!("repository has no workdir"))?;
            fs::write(workdir.join(file), content)
                .with_context(|| format!("write worktree file {file}"))?;

            let mut index = self.repo.index().context("open index")?;
            index
                .add_path(Path::new(file))
                .with_context(|| format!("add {file} to index"))?;
            let tree_oid = index.write_tree().context("write tree")?;
            index.write().context("write index")?;
            let tree = self.repo.find_tree(tree_oid).context("find tree")?;
            let signature = Signature::now("Test", "test@example.com").context("signature")?;

            let parents: Vec<&git2::Commit<'_>> = parent.into_iter().collect();
            let oid = self
                .repo
                .commit(
                    Some("HEAD"),
                    &signature,
                    &signature,
                    message,
                    &tree,
                    &parents,
                )
                .with_context(|| format!("commit {message}"))?;
            Ok(oid)
        }
    }
}
