//! Where AgentsMon keeps its files. The application and the hook helper must
//! agree on these directories, so both resolve them here.

use directories::ProjectDirs;

/// Data and configuration directories, named after the bundle identifier
/// `fr.gmbl.AgentsMon`.
pub fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("fr", "gmbl", "AgentsMon")
}
