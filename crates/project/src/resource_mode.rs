use collections::HashMap;
use gpui::{App, Global};
use util::path_list::PathList;

/// How much a project runs in the background.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ResourceMode {
    /// Indexing, git, language servers and everything else run.
    #[default]
    Full,
    /// Only what the AI agent needs runs. The file tree is read on demand, and git
    /// and language servers stay off until the project is switched to
    /// [`ResourceMode::Full`].
    AgentOnly,
    /// The file tree is read and git is tracked, but language servers stay off.
    /// What the git panel needs, without paying for language servers.
    Git,
}

impl ResourceMode {
    /// Whether the project reads its files and tracks git repositories.
    pub fn tracks_git(self) -> bool {
        !matches!(self, ResourceMode::AgentOnly)
    }

    /// Whether a project in this mode runs everything `needed` asks for.
    pub fn covers(self, needed: ResourceMode) -> bool {
        (!needed.tracks_git() || self.tracks_git())
            && (!needed.runs_language_servers() || self.runs_language_servers())
    }

    /// Whether the project runs language servers.
    pub fn runs_language_servers(self) -> bool {
        self == ResourceMode::Full
    }
}

#[derive(Default)]
struct RequestedModes(HashMap<PathList, ResourceMode>);

impl Global for RequestedModes {}

/// Modes asked for before a project exists. Whoever is about to open a workspace
/// for some folders asks here, and the workspace takes the answer when it creates
/// its project, ahead of the first worktree being added: that is the only moment
/// the scanner can still be kept from starting.
pub struct ResourceModeRequests;

impl ResourceModeRequests {
    pub fn request(paths: &PathList, mode: ResourceMode, cx: &mut App) {
        cx.default_global::<RequestedModes>()
            .0
            .insert(paths.clone(), mode);
    }

    pub fn take(paths: &PathList, cx: &mut App) -> Option<ResourceMode> {
        cx.default_global::<RequestedModes>().0.remove(paths)
    }
}
