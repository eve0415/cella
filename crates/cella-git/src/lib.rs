mod branch;
mod cmd;
pub mod content_hash;
mod error;
mod repo;
mod sanitize;
#[cfg(test)]
mod test_utils;
mod worktree;

pub use branch::{BranchState, is_tracking_gone, merged_branches, resolve_branch};
pub use error::CellaGitError;
pub use repo::{RepoInfo, default_branch, discover, is_inside_container};
pub use sanitize::branch_to_dir_name;
pub use worktree::{WorktreeInfo, create, list, remove, worktree_path};
