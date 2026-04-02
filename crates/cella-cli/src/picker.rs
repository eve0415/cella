//! Interactive fuzzy-search picker for branch and container selection.
//!
//! Provides a shared picker abstraction used by commands that need to resolve
//! a worktree branch or container target interactively.

use std::collections::HashMap;
use std::fmt::{self, Write};
use std::io::IsTerminal;

use cella_backend::{ContainerInfo, ContainerState};
use cella_git::WorktreeInfo;
use owo_colors::OwoColorize;

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// A single candidate for the fuzzy picker.
pub struct PickerItem<T> {
    /// Display label shown in the picker.
    label: String,
    /// The underlying value returned on selection.
    pub value: T,
}

impl<T> fmt::Display for PickerItem<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}

/// The outcome of a picker attempt.
pub enum PickResult<T> {
    /// User selected an item.
    Selected(T),
    /// User cancelled (Esc / Ctrl-C).
    Cancelled,
}

/// Error returned when the picker cannot be shown or has no candidates.
#[derive(Debug)]
pub enum PickerError {
    NoCandidates,
    NonInteractive {
        message: String,
        candidates: Vec<String>,
    },
}

impl fmt::Display for PickerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCandidates => f.write_str("no candidates available"),
            Self::NonInteractive { message, .. } => f.write_str(message),
        }
    }
}

impl std::error::Error for PickerError {}

// ---------------------------------------------------------------------------
// Generic picker
// ---------------------------------------------------------------------------

/// Show an interactive fuzzy-search picker.
///
/// `prompt` is displayed above the picker (e.g., "Select a branch:").
/// `initial_filter` pre-fills the search box (e.g., the user's typo).
///
/// Returns `PickerError::NonInteractive` if stderr is not a TTY.
///
/// # Errors
///
/// Returns `PickerError::NoCandidates` if `items` is empty.
/// Returns `PickerError::NonInteractive` if stderr is not a TTY.
pub fn pick<T>(
    prompt: &str,
    items: Vec<PickerItem<T>>,
    initial_filter: Option<&str>,
) -> Result<PickResult<T>, PickerError> {
    if items.is_empty() {
        return Err(PickerError::NoCandidates);
    }

    if !std::io::stderr().is_terminal() {
        let candidates: Vec<String> = items.iter().map(|i| i.label.clone()).collect();
        return Err(PickerError::NonInteractive {
            message: prompt.to_string(),
            candidates,
        });
    }

    let mut select = inquire::Select::new(prompt, items);
    if let Some(filter) = initial_filter {
        select = select.with_starting_filter_input(filter);
    }

    match select.prompt_skippable() {
        Ok(Some(item)) => Ok(PickResult::Selected(item.value)),
        Ok(None)
        | Err(
            inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted,
        ) => Ok(PickResult::Cancelled),
        Err(e) => Err(PickerError::NonInteractive {
            message: format!("Prompt error: {e}"),
            candidates: vec![],
        }),
    }
}

// ---------------------------------------------------------------------------
// JSON error helper
// ---------------------------------------------------------------------------

/// Format a JSON error response with candidates.
pub fn json_candidates_error(message: &str, candidates: &[String]) -> serde_json::Value {
    serde_json::json!({
        "error": message,
        "candidates": candidates,
    })
}

// ---------------------------------------------------------------------------
// Worktree picker
// ---------------------------------------------------------------------------

/// Build picker items from worktrees with status indicators.
///
/// Each item is labeled: `branch_name  {indicator}` where:
/// - `●` green = running container
/// - `○` gray = stopped container
/// - `★` yellow = main worktree
/// - `·` gray = no container
pub fn worktree_picker_items<S: std::hash::BuildHasher>(
    worktrees: &[WorktreeInfo],
    container_states: &HashMap<String, ContainerState, S>,
    exclude_branch: Option<&str>,
) -> Vec<PickerItem<WorktreeInfo>> {
    worktrees
        .iter()
        .filter(|wt| {
            let branch = wt.branch.as_deref();
            // Skip worktrees without a branch (detached HEAD)
            branch.is_some() && branch != exclude_branch
        })
        .map(|wt| {
            let branch = wt.branch.as_deref().unwrap_or("(detached)");
            let label = format_worktree_label(branch, wt.is_main, container_states.get(branch));
            PickerItem {
                label,
                value: wt.clone(),
            }
        })
        .collect()
}

/// Format a worktree picker label with status indicator.
fn format_worktree_label(
    branch: &str,
    is_main: bool,
    container_state: Option<&ContainerState>,
) -> String {
    if is_main {
        return format!("{branch}  {}", "★".yellow());
    }
    match container_state {
        Some(ContainerState::Running) => format!("{branch}  {}", "●".green()),
        Some(ContainerState::Stopped) => format!("{branch}  {}", "○".dimmed()),
        Some(_) => format!("{branch}  {}", "?".dimmed()),
        None => format!("{branch}  {}", "·".dimmed()),
    }
}

/// Resolve a branch name to a worktree, with fuzzy picker fallback.
///
/// Three trigger cases:
/// 1. `name` is `None` — show picker with all worktrees
/// 2. `name` matches exactly — return that worktree
/// 3. `name` does not match — show picker pre-filtered with the input
///
/// # Errors
///
/// Returns an error if no worktree branches are available, the picker
/// cannot be shown, or the user cancels.
pub fn resolve_worktree_interactive<S: std::hash::BuildHasher>(
    worktrees: &[WorktreeInfo],
    container_states: &HashMap<String, ContainerState, S>,
    name: Option<&str>,
    exclude_branch: Option<&str>,
) -> Result<WorktreeInfo, Box<dyn std::error::Error>> {
    // Try exact match first
    if let Some(name) = name
        && let Some(wt) = worktrees
            .iter()
            .find(|wt| wt.branch.as_deref() == Some(name))
    {
        return Ok(wt.clone());
    }

    let items = worktree_picker_items(worktrees, container_states, exclude_branch);

    if items.is_empty() {
        return Err("No worktree branches available".into());
    }

    if let Some(name) = name {
        eprintln!("No worktree found for branch '{name}'.");
    }

    match pick("Select a branch:", items, name)? {
        PickResult::Selected(wt) => Ok(wt),
        PickResult::Cancelled => Err("Selection cancelled".into()),
    }
}

// ---------------------------------------------------------------------------
// Container picker
// ---------------------------------------------------------------------------

/// Build picker items from containers with state indicators.
///
/// Each item is labeled: `container_name (branch)  {state_indicator}`
pub fn container_picker_items(
    containers: &[ContainerInfo],
    exclude_name: Option<&str>,
) -> Vec<PickerItem<ContainerInfo>> {
    containers
        .iter()
        .filter(|c| exclude_name.is_none_or(|name| c.name != name))
        .map(|c| {
            let label = format_container_label(c);
            PickerItem {
                label,
                value: c.clone(),
            }
        })
        .collect()
}

/// Format a container picker label with state indicator.
fn format_container_label(container: &ContainerInfo) -> String {
    let branch = container
        .labels
        .get("dev.cella.branch")
        .map_or("-", String::as_str);
    let indicator = match &container.state {
        ContainerState::Running => format!("{}", "●".green()),
        ContainerState::Stopped => format!("{}", "○".dimmed()),
        _ => format!("{}", "?".dimmed()),
    };
    format!("{} ({branch})  {indicator}", container.name)
}

/// Resolve a container via interactive picker.
///
/// Shows containers in the picker. Called when normal resolution fails.
///
/// # Errors
///
/// Returns an error if no containers are available, the picker
/// cannot be shown, or the user cancels.
pub fn resolve_container_interactive(
    containers: &[ContainerInfo],
    exclude_name: Option<&str>,
    prompt: &str,
    initial_filter: Option<&str>,
) -> Result<ContainerInfo, Box<dyn std::error::Error>> {
    let items = container_picker_items(containers, exclude_name);

    if items.is_empty() {
        return Err("No cella containers found".into());
    }

    match pick(prompt, items, initial_filter)? {
        PickResult::Selected(c) => Ok(c),
        PickResult::Cancelled => Err("Selection cancelled".into()),
    }
}

/// Build a map of branch name to container state from a list of containers.
pub fn branch_container_states(containers: &[ContainerInfo]) -> HashMap<String, ContainerState> {
    let mut map = HashMap::new();
    for c in containers {
        if let Some(branch) = c.labels.get("dev.cella.branch") {
            map.insert(branch.clone(), c.state.clone());
        }
    }
    map
}

/// Format a non-interactive error message with a list of candidates.
pub fn format_non_interactive_error(message: &str, candidates: &[String]) -> String {
    let mut out = format!("Error: {message}\n\nAvailable options:");
    for c in candidates {
        let _ = write!(out, "\n  {c}");
    }
    out
}

// ---------------------------------------------------------------------------
// UpArgs workspace picker
// ---------------------------------------------------------------------------

/// Pre-flight worktree picker for commands that embed `UpArgs`.
///
/// If the user provided no explicit `workspace_folder` or `branch`, and the CWD
/// is in a repo with multiple worktrees, offer a picker to choose which worktree
/// to target. Sets `up_args.workspace_folder` to the selected worktree path.
pub async fn resolve_up_workspace(up_args: &mut crate::commands::up::UpArgs) {
    if up_args.workspace_folder.is_some() || up_args.branch.is_some() {
        return;
    }

    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let Ok(repo_info) = cella_git::discover(&cwd) else {
        return;
    };
    let Ok(worktrees) = cella_git::list(&repo_info.root) else {
        return;
    };

    // Only offer picker if there are multiple worktrees
    if worktrees.len() <= 1 {
        return;
    }

    // Try to connect to Docker for container status
    let container_states = if let Ok(client) = crate::backend::resolve_backend(None, None) {
        let containers = client
            .list_cella_containers(false)
            .await
            .unwrap_or_default();
        branch_container_states(&containers)
    } else {
        HashMap::new()
    };

    let items = worktree_picker_items(
        &worktrees,
        &container_states,
        repo_info.head_branch.as_deref(),
    );

    if items.is_empty() {
        return;
    }

    if let Ok(PickResult::Selected(wt)) = pick("Select a branch:", items, None) {
        up_args.workspace_folder = Some(wt.path);
    }
}

// ---------------------------------------------------------------------------
// Container target helper
// ---------------------------------------------------------------------------

/// Check if any explicit container targeting flags were provided.
///
/// Returns `true` if the user provided at least one targeting flag,
/// meaning we should NOT fall back to the interactive picker on error.
pub const fn has_explicit_target(target: &cella_backend::ContainerTarget) -> bool {
    target.container_id.is_some()
        || target.container_name.is_some()
        || target.id_label.is_some()
        || target.workspace_folder.is_some()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use cella_backend::BackendKind;
    use cella_git::WorktreeInfo;

    use super::*;

    fn make_worktree(branch: &str, is_main: bool) -> WorktreeInfo {
        WorktreeInfo {
            path: format!("/repo/{branch}").into(),
            head: "abc123".to_string(),
            branch: Some(branch.to_string()),
            is_main,
        }
    }

    fn make_container(name: &str, branch: &str, state: ContainerState) -> ContainerInfo {
        let mut labels = HashMap::new();
        labels.insert("dev.cella.branch".to_string(), branch.to_string());
        ContainerInfo {
            id: format!("id-{name}"),
            name: name.to_string(),
            state,
            exit_code: None,
            labels,
            config_hash: None,
            ports: vec![],
            created_at: None,
            container_user: None,
            image: None,
            mounts: vec![],
            backend: BackendKind::Docker,
        }
    }

    #[test]
    fn worktree_items_excludes_current_branch() {
        let worktrees = vec![
            make_worktree("main", true),
            make_worktree("feat/auth", false),
            make_worktree("feat/api", false),
        ];
        let states = HashMap::new();

        let items = worktree_picker_items(&worktrees, &states, Some("main"));
        assert_eq!(items.len(), 2);
        assert!(
            items
                .iter()
                .all(|i| i.value.branch.as_deref() != Some("main"))
        );
    }

    #[test]
    fn worktree_items_no_exclusion() {
        let worktrees = vec![
            make_worktree("main", true),
            make_worktree("feat/auth", false),
        ];
        let states = HashMap::new();

        let items = worktree_picker_items(&worktrees, &states, None);
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn worktree_items_skips_detached_head() {
        let mut detached = make_worktree("detached", false);
        detached.branch = None;
        let worktrees = vec![make_worktree("main", true), detached];
        let states = HashMap::new();

        let items = worktree_picker_items(&worktrees, &states, None);
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn worktree_items_empty() {
        let items: Vec<PickerItem<WorktreeInfo>> =
            worktree_picker_items(&[], &HashMap::new(), None);
        assert!(items.is_empty());
    }

    #[test]
    fn worktree_label_main_has_star() {
        let label = format_worktree_label("main", true, None);
        assert!(label.contains("main"));
        assert!(label.contains('\u{2605}')); // ★
    }

    #[test]
    fn worktree_label_running() {
        let label = format_worktree_label("feat", false, Some(&ContainerState::Running));
        assert!(label.contains('\u{25cf}')); // ●
    }

    #[test]
    fn worktree_label_stopped() {
        let label = format_worktree_label("feat", false, Some(&ContainerState::Stopped));
        assert!(label.contains('\u{25cb}')); // ○
    }

    #[test]
    fn worktree_label_no_container() {
        let label = format_worktree_label("feat", false, None);
        assert!(label.contains('\u{00b7}')); // ·
    }

    #[test]
    fn container_items_excludes_by_name() {
        let containers = vec![
            make_container("dev-main", "main", ContainerState::Running),
            make_container("dev-feat", "feat", ContainerState::Running),
        ];

        let items = container_picker_items(&containers, Some("dev-main"));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].value.name, "dev-feat");
    }

    #[test]
    fn container_items_no_exclusion() {
        let containers = vec![
            make_container("dev-main", "main", ContainerState::Running),
            make_container("dev-feat", "feat", ContainerState::Stopped),
        ];

        let items = container_picker_items(&containers, None);
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn container_label_format() {
        let c = make_container("my-dev", "feat/auth", ContainerState::Running);
        let label = format_container_label(&c);
        assert!(label.contains("my-dev"));
        assert!(label.contains("feat/auth"));
        assert!(label.contains('\u{25cf}')); // ●
    }

    #[test]
    fn branch_container_states_map() {
        let containers = vec![
            make_container("c1", "main", ContainerState::Running),
            make_container("c2", "feat", ContainerState::Stopped),
        ];

        let map = branch_container_states(&containers);
        assert_eq!(map.get("main"), Some(&ContainerState::Running));
        assert_eq!(map.get("feat"), Some(&ContainerState::Stopped));
    }

    #[test]
    fn json_candidates_error_structure() {
        let result = json_candidates_error("not found", &["main".to_string(), "feat".to_string()]);
        assert_eq!(result["error"], "not found");
        assert_eq!(result["candidates"][0], "main");
        assert_eq!(result["candidates"][1], "feat");
    }

    #[test]
    fn pick_returns_no_candidates_for_empty_list() {
        let result: Result<PickResult<String>, PickerError> = pick("test", vec![], None);
        assert!(matches!(result, Err(PickerError::NoCandidates)));
    }

    #[test]
    fn resolve_worktree_exact_match() {
        let worktrees = vec![
            make_worktree("main", true),
            make_worktree("feat/auth", false),
        ];
        let states = HashMap::new();

        let result =
            resolve_worktree_interactive(&worktrees, &states, Some("feat/auth"), Some("main"));
        assert!(result.is_ok());
        assert_eq!(result.unwrap().branch.as_deref(), Some("feat/auth"));
    }

    #[test]
    fn format_non_interactive_includes_candidates() {
        let msg = format_non_interactive_error("No match", &["a".into(), "b".into()]);
        assert!(msg.contains("No match"));
        assert!(msg.contains("  a"));
        assert!(msg.contains("  b"));
    }

    #[test]
    fn format_non_interactive_empty_candidates() {
        let msg = format_non_interactive_error("Nothing", &[]);
        assert!(msg.contains("Nothing"));
        assert!(msg.contains("Available options:"));
    }

    #[test]
    fn format_non_interactive_single_candidate() {
        let msg = format_non_interactive_error("Pick one", &["only".into()]);
        assert!(msg.contains("  only"));
    }

    #[test]
    fn worktree_label_other_state_shows_question_mark() {
        let label = format_worktree_label(
            "feat",
            false,
            Some(&ContainerState::Other("restarting".to_string())),
        );
        assert!(label.contains('?'));
    }

    #[test]
    fn container_label_stopped_state() {
        let c = make_container("dev-main", "main", ContainerState::Stopped);
        let label = format_container_label(&c);
        assert!(label.contains("dev-main"));
        assert!(label.contains("main"));
        assert!(label.contains('\u{25cb}')); // ○
    }

    #[test]
    fn container_label_other_state() {
        let mut c = make_container("dev-other", "feat", ContainerState::Running);
        c.state = ContainerState::Other("paused".to_string());
        let label = format_container_label(&c);
        assert!(label.contains('?'));
    }

    #[test]
    fn container_label_no_branch_label() {
        let c = ContainerInfo {
            id: "id-1".to_string(),
            name: "no-branch".to_string(),
            state: ContainerState::Running,
            exit_code: None,
            labels: HashMap::new(),
            config_hash: None,
            ports: vec![],
            created_at: None,
            container_user: None,
            image: None,
            mounts: vec![],
            backend: BackendKind::Docker,
        };
        let label = format_container_label(&c);
        assert!(label.contains('-')); // branch defaults to "-"
    }

    #[test]
    fn branch_container_states_skips_without_branch_label() {
        let c = ContainerInfo {
            id: "id-1".to_string(),
            name: "no-label".to_string(),
            state: ContainerState::Running,
            exit_code: None,
            labels: HashMap::new(),
            config_hash: None,
            ports: vec![],
            created_at: None,
            container_user: None,
            image: None,
            mounts: vec![],
            backend: BackendKind::Docker,
        };
        let map = branch_container_states(&[c]);
        assert!(map.is_empty());
    }

    #[test]
    fn branch_container_states_empty_input() {
        let map = branch_container_states(&[]);
        assert!(map.is_empty());
    }

    #[test]
    fn has_explicit_target_none() {
        let target = cella_backend::ContainerTarget {
            container_id: None,
            container_name: None,
            id_label: None,
            workspace_folder: None,
        };
        assert!(!has_explicit_target(&target));
    }

    #[test]
    fn has_explicit_target_container_id() {
        let target = cella_backend::ContainerTarget {
            container_id: Some("abc123".to_string()),
            container_name: None,
            id_label: None,
            workspace_folder: None,
        };
        assert!(has_explicit_target(&target));
    }

    #[test]
    fn has_explicit_target_container_name() {
        let target = cella_backend::ContainerTarget {
            container_id: None,
            container_name: Some("my-container".to_string()),
            id_label: None,
            workspace_folder: None,
        };
        assert!(has_explicit_target(&target));
    }

    #[test]
    fn has_explicit_target_id_label() {
        let target = cella_backend::ContainerTarget {
            container_id: None,
            container_name: None,
            id_label: Some("dev.cella.id=abc".to_string()),
            workspace_folder: None,
        };
        assert!(has_explicit_target(&target));
    }

    #[test]
    fn has_explicit_target_workspace_folder() {
        let target = cella_backend::ContainerTarget {
            container_id: None,
            container_name: None,
            id_label: None,
            workspace_folder: Some("/workspaces/myapp".into()),
        };
        assert!(has_explicit_target(&target));
    }

    #[test]
    fn picker_error_display_no_candidates() {
        let err = PickerError::NoCandidates;
        assert_eq!(err.to_string(), "no candidates available");
    }

    #[test]
    fn picker_error_display_non_interactive() {
        let err = PickerError::NonInteractive {
            message: "Select a branch:".to_string(),
            candidates: vec!["main".to_string(), "feat".to_string()],
        };
        assert_eq!(err.to_string(), "Select a branch:");
    }

    #[test]
    fn picker_error_is_std_error() {
        let err: Box<dyn std::error::Error> = Box::new(PickerError::NoCandidates);
        assert_eq!(err.to_string(), "no candidates available");
    }

    #[test]
    fn picker_item_display() {
        let item: PickerItem<String> = PickerItem {
            label: "my-label".to_string(),
            value: "my-value".to_string(),
        };
        assert_eq!(format!("{item}"), "my-label");
    }

    #[test]
    fn json_candidates_error_empty() {
        let result = json_candidates_error("empty", &[]);
        assert_eq!(result["error"], "empty");
        assert!(result["candidates"].as_array().unwrap().is_empty());
    }

    #[test]
    fn container_picker_items_empty() {
        let items = container_picker_items(&[], None);
        assert!(items.is_empty());
    }

    #[test]
    fn worktree_items_all_detached() {
        let mut wt1 = make_worktree("a", false);
        wt1.branch = None;
        let mut wt2 = make_worktree("b", false);
        wt2.branch = None;
        let items = worktree_picker_items(&[wt1, wt2], &HashMap::new(), None);
        assert!(items.is_empty());
    }

    #[test]
    fn worktree_items_with_running_container_state() {
        let worktrees = vec![make_worktree("feat/auth", false)];
        let mut states = HashMap::new();
        states.insert("feat/auth".to_string(), ContainerState::Running);

        let items = worktree_picker_items(&worktrees, &states, None);
        assert_eq!(items.len(), 1);
        // Label should contain the running indicator
        assert!(items[0].label.contains('\u{25cf}')); // ●
    }

    #[test]
    fn resolve_worktree_exact_match_returns_correct_path() {
        let worktrees = vec![make_worktree("main", true), make_worktree("feat/x", false)];
        let states = HashMap::new();

        let result = resolve_worktree_interactive(&worktrees, &states, Some("main"), None);
        assert!(result.is_ok());
        let wt = result.unwrap();
        assert_eq!(wt.branch.as_deref(), Some("main"));
        assert!(wt.is_main);
    }

    #[test]
    fn resolve_worktree_no_match_empty_worktrees() {
        let states = HashMap::new();
        let result = resolve_worktree_interactive(&[], &states, Some("nonexistent"), None);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No worktree branches")
        );
    }
}
