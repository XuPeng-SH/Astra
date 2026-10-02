use super::ToolExecutor;
use serde_json::json;

pub(crate) fn init_temp_git_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp repo");
    std::process::Command::new("git")
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("git init");
    std::process::Command::new("git")
        .args(["config", "user.name", "Test User"])
        .current_dir(dir.path())
        .output()
        .expect("git config user.name");
    std::process::Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(dir.path())
        .output()
        .expect("git config user.email");
    std::fs::write(dir.path().join("tracked.txt"), "committed\n").expect("seed tracked file");
    std::process::Command::new("git")
        .args(["add", "tracked.txt"])
        .current_dir(dir.path())
        .output()
        .expect("git add");
    std::process::Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(dir.path())
        .output()
        .expect("git commit");
    dir
}

// ── extract_github_owner_repo edge cases ──

// ── detect_git_remote_repos ──

// ── add_preferred_repo / get_preferred_repos ──

// ── Worktree session tests ────────────────────────────────────────────────

#[test]
fn worktree_session_initial_state() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());
    assert!(!exe.in_worktree_session());
    assert!(exe.get_worktree_session().is_none());
    assert_eq!(exe.effective_project_root(), dir.path());
}

#[test]
fn enter_and_exit_worktree_error_paths() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    // empty branch
    let result = exe.enter_worktree("");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("required"));

    // shell injection
    for dangerous in &["test;rm", "test|cat", "test&", "test`id`", "$(whoami)"] {
        let result = exe.enter_worktree(dangerous);
        assert!(
            result.is_err(),
            "should reject dangerous branch: {dangerous}"
        );
        assert!(result.unwrap_err().contains("Invalid"));
    }

    // exit when not in session
    let result = exe.exit_worktree("keep", false);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Not in a worktree session"));
}

#[test]
fn git_worktree_error_paths() {
    let dir = tempfile::tempdir().unwrap();
    let exe = ToolExecutor::new(dir.path());

    // enter without branch
    let result = exe.worktree(&json!({"action": "enter"}));
    assert!(result.contains("Error"));
    assert!(result.contains("branch"));

    // exit when not in session
    let result = exe.worktree(&json!({"action": "exit"}));
    assert!(result.contains("Error"));
    assert!(result.contains("Not in a worktree session"));
}

#[tokio::test]
async fn git_worktree_enter_records_rollback_handle() {
    let dir = init_temp_git_repo();
    let exe = ToolExecutor::new(dir.path());
    exe.journal_turn_index
        .store(7, std::sync::atomic::Ordering::Relaxed);

    let outcome = exe.worktree_with_metadata(&json!({
        "action": "enter",
        "branch": "session-demo",
    }));
    assert!(
        !outcome.output.starts_with("Error:"),
        "enter failed: {}",
        outcome.output
    );
    assert!(exe.in_worktree_session(), "should enter worktree session");

    let listed = exe
        .rollback_recorded_turn_mutations(&json!({"scope": "list"}))
        .await;
    let listed_json: serde_json::Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(listed_json["total_git_worktree_entries"].as_u64(), Some(1));

    let cleanup = exe.worktree(&json!({
        "action": "exit",
        "exit_action": "remove",
        "discard_changes": true,
    }));
    assert!(!cleanup.starts_with("Error:"), "cleanup failed: {cleanup}");
}

#[tokio::test]
async fn session_worktree_tool_enters_and_exits_through_public_dispatch() {
    let dir = init_temp_git_repo();
    let exe = ToolExecutor::new(dir.path());
    let entered = exe
        .execute_with_metadata(
            "worktree",
            &json!({"action":"enter", "branch":"session-lifecycle"}),
        )
        .await;
    assert!(!entered.is_error, "{entered:?}");
    let session = exe.get_worktree_session().expect("session switched");
    assert!(session.worktree_path.join("tracked.txt").exists());
    // A dirty linked worktree must not be deleted through an incomplete or ignored status query.
    std::fs::write(session.worktree_path.join("tracked.txt"), "changed\n").unwrap();
    let denied = exe
        .execute_with_metadata(
            "worktree",
            &json!({"action":"exit", "exit_action":"remove"}),
        )
        .await;
    assert!(denied.is_error, "{denied:?}");
    assert!(exe.in_worktree_session());
    let exited = exe
        .execute_with_metadata(
            "worktree",
            &json!({"action":"exit", "exit_action":"remove", "discard_changes":true}),
        )
        .await;
    assert!(!exited.is_error, "{exited:?}");
    assert!(!exe.in_worktree_session());
    assert!(!session.worktree_path.exists());
    let invalid = exe
        .execute_with_metadata("worktree", &json!({"action":"push", "branch":"main"}))
        .await;
    assert!(invalid.is_error);
}

#[test]
fn child_worktrees_isolate_tracked_sources_and_preserve_the_selected_parent() {
    use crate::edge_tools::worktree::{cleanup_child_worktree, provision_child_worktree};
    let repo = init_temp_git_repo();
    let parent = ToolExecutor::new(repo.path());
    let token = tokio_util::sync::CancellationToken::new();
    let mut children = Vec::new();
    for _ in 0..2 {
        let (handle, result) =
            provision_child_worktree(repo.path(), &uuid::Uuid::new_v4().to_string(), &token);
        result.expect("create real detached worktree");
        let handle = handle.expect("workspace owner");
        assert_eq!(
            std::fs::read_to_string(handle.path.join("tracked.txt")).unwrap(),
            "committed\n"
        );
        std::fs::write(handle.path.join("tracked.txt"), "child edit\n").unwrap();
        children.push(handle);
    }
    assert_ne!(children[0].path, children[1].path);
    assert_eq!(
        std::fs::read_to_string(repo.path().join("tracked.txt")).unwrap(),
        "committed\n"
    );
    assert_eq!(parent.effective_project_root(), repo.path());
    for mut child in children {
        cleanup_child_worktree(&mut child, &token).expect("Git removes the exact child workspace");
        assert!(!child.path.exists());
    }
    assert_eq!(parent.effective_project_root(), repo.path());
}

#[test]
fn child_workspace_failures_never_succeed_with_an_empty_directory() {
    use crate::edge_tools::worktree::{cleanup_child_worktree, provision_child_worktree};
    let token = tokio_util::sync::CancellationToken::new();
    let repo = tempfile::tempdir().unwrap();
    let run = uuid::Uuid::new_v4().to_string();
    let (handle, result) = provision_child_worktree(repo.path(), &run, &token);
    assert!(result.is_err(), "non-Git workspace must reject");
    assert!(handle.is_none());
    assert!(!repo.path().join(".agent-worktrees").join(&run).exists());
    let status = std::process::Command::new("git")
        .arg("init")
        .current_dir(repo.path())
        .status()
        .unwrap();
    assert!(status.success());
    let (handle, result) = provision_child_worktree(repo.path(), &run, &token);
    assert!(result.is_err(), "repository without HEAD must reject");
    if let Some(mut handle) = handle {
        cleanup_child_worktree(&mut handle, &token).unwrap();
    }
    assert!(!repo.path().join(".agent-worktrees").join(run).exists());
}

#[test]
fn cancelled_child_provisioning_does_not_start_git_or_remove_existing_directories() {
    use crate::edge_tools::worktree::provision_child_worktree;
    let repo = init_temp_git_repo();
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    let run = uuid::Uuid::new_v4().to_string();
    assert!(
        provision_child_worktree(repo.path(), &run, &token)
            .1
            .is_err()
    );
    assert!(!repo.path().join(".agent-worktrees").join(&run).exists());
    let path = repo.path().join(".agent-worktrees").join(&run);
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("existing.txt"), "user-owned").unwrap();
    let (handle, result) = provision_child_worktree(
        repo.path(),
        &run,
        &tokio_util::sync::CancellationToken::new(),
    );
    assert!(result.is_err());
    assert!(handle.is_none());
    assert_eq!(
        std::fs::read_to_string(path.join("existing.txt")).unwrap(),
        "user-owned"
    );
}

#[test]
fn child_cleanup_removes_git_registration_when_directory_is_already_missing() {
    use crate::edge_tools::worktree::{cleanup_child_worktree, provision_child_worktree};
    let repo = init_temp_git_repo();
    let cancel = tokio_util::sync::CancellationToken::new();
    let (handle, result) =
        provision_child_worktree(repo.path(), &uuid::Uuid::new_v4().to_string(), &cancel);
    result.expect("provision child");
    let mut handle = handle.unwrap();
    std::fs::remove_dir_all(&handle.path).unwrap();
    cleanup_child_worktree(&mut handle, &cancel).expect("remove missing workspace registration");
    cleanup_child_worktree(&mut handle, &cancel).expect("settled cleanup retry");
    let listed = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain", "-z"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(listed.status.success());
    assert!(
        !listed
            .stdout
            .split(|byte| *byte == 0)
            .any(|field| field.strip_prefix(b"worktree ")
                == Some(handle.path.to_str().unwrap().as_bytes()))
    );
}
