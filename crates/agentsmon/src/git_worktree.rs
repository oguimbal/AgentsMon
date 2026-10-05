//! Attach sessions running in a git worktree to the card of its main checkout.
//!
//! Reads only git's own `.git` link files (`gitdir:` and `commondir`), never
//! spawns `git`, and caches the answer per working directory.

use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use agentsmon_core::AgentSnapshot;

#[derive(Default)]
pub struct WorktreeResolver {
    cache: HashMap<PathBuf, Option<PathBuf>>,
}

impl WorktreeResolver {
    /// Fill `group_dir` of every snapshot whose working directory lies in a
    /// linked worktree. Cache entries of directories no longer seen are dropped.
    pub fn resolve(&mut self, snapshots: &mut [AgentSnapshot]) {
        self.cache
            .retain(|cwd, _| snapshots.iter().any(|snapshot| snapshot.cwd == *cwd));
        for snapshot in snapshots {
            snapshot.group_dir = self
                .cache
                .entry(snapshot.cwd.clone())
                .or_insert_with(|| main_checkout_dir(&snapshot.cwd))
                .clone();
        }
    }
}

/// For a directory inside a linked worktree, the same directory inside the
/// main checkout. `None` for main checkouts, plain directories, submodules,
/// worktrees of bare repositories and relative (synthetic) paths.
fn main_checkout_dir(cwd: &Path) -> Option<PathBuf> {
    if !cwd.is_absolute() {
        return None;
    }
    for root in cwd.ancestors() {
        let dot_git = root.join(".git");
        let Ok(metadata) = fs::metadata(&dot_git) else {
            continue;
        };
        if !metadata.is_file() {
            return None;
        }
        let link = fs::read_to_string(&dot_git).ok()?;
        let git_dir = root.join(link.trim().strip_prefix("gitdir:")?.trim());
        // Submodules link to a git dir without `commondir`.
        let common = fs::read_to_string(git_dir.join("commondir")).ok()?;
        let common_dir = normalize(&git_dir.join(common.trim()));
        if common_dir.file_name()? != ".git" {
            return None;
        }
        let main = common_dir.parent()?;
        return Some(main.join(cwd.strip_prefix(root).ok()?));
    }
    None
}

/// Collapse `.` and `..` without resolving symlinks, so the result matches the
/// working directory a session in the main checkout reports.
fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A main checkout at `<tmp>/repo` with a worktree at `<tmp>/repo/.claude/worktrees/wt`.
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let worktree = repo.join(".claude/worktrees/wt");
        let git_dir = repo.join(".git/worktrees/wt");
        fs::create_dir_all(&git_dir).unwrap();
        fs::create_dir_all(worktree.join("sub")).unwrap();
        fs::write(git_dir.join("commondir"), "../..\n").unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .unwrap();
        (tmp, repo, worktree)
    }

    #[test]
    fn worktree_maps_to_its_main_checkout() {
        let (_tmp, repo, worktree) = fixture();
        assert_eq!(main_checkout_dir(&worktree), Some(repo.clone()));
        assert_eq!(
            main_checkout_dir(&worktree.join("sub")),
            Some(repo.join("sub"))
        );
    }

    #[test]
    fn relative_gitdir_link_is_resolved() {
        let (_tmp, repo, worktree) = fixture();
        fs::write(worktree.join(".git"), "gitdir: ../../../.git/worktrees/wt").unwrap();
        assert_eq!(main_checkout_dir(&worktree), Some(repo));
    }

    #[test]
    fn main_checkouts_and_other_directories_keep_their_own_group() {
        let (tmp, repo, _worktree) = fixture();
        assert_eq!(main_checkout_dir(&repo), None);
        assert_eq!(main_checkout_dir(tmp.path()), None);
        assert_eq!(main_checkout_dir(&tmp.path().join("missing")), None);
        assert_eq!(main_checkout_dir(Path::new("openclaw/ops")), None);
    }

    #[test]
    fn submodules_are_not_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let module = tmp.path().join("repo/lib");
        let git_dir = tmp.path().join("repo/.git/modules/lib");
        fs::create_dir_all(&module).unwrap();
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(module.join(".git"), "gitdir: ../.git/modules/lib").unwrap();
        assert_eq!(main_checkout_dir(&module), None);
    }
}
