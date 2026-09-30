mod claude;
mod claude_runtime;
mod codex;
#[cfg(unix)]
mod codex_ipc;
mod focus;
pub mod openclaw;
mod process;
pub mod vibe;

use std::path::Path;

use agentsmon_core::{AgentSnapshot, FocusTarget, ProviderUsage};
use anyhow::Result;

pub use claude::{CStatusRecord, ClaudeProvider, ProviderWatcher};
pub use codex::{CodexApprovalsReviewer, CodexProvider, CodexStatusRecord};
pub use openclaw::OpenClawProvider;
pub use vibe::{VibeProvider, VibeStatusRecord};

/// Provider-owned, content-free diagnostic. `details` must never contain remote
/// payloads, tool arguments/results, conversation text, or credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDiagnostic {
    pub code: &'static str,
    pub summary: &'static str,
    pub details: String,
}

pub trait AgentProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn display_name(&self) -> &'static str {
        self.id()
    }
    fn icon_png(&self) -> Option<&'static [u8]> {
        None
    }
    /// Brand colour (0xRRGGBB) drawn as a ring around the context indicator of
    /// this provider's sessions, so agents on the same repository stay apart.
    fn accent_rgb(&self) -> Option<u32> {
        None
    }
    /// A provider that needs explicit setup (e.g. a remote gateway) stays out
    /// of the bar until configured.
    fn is_enabled(&self) -> bool {
        true
    }
    fn discover(&self) -> Result<Vec<AgentSnapshot>>;
    fn usage(&self) -> Result<Option<ProviderUsage>>;
    fn focus(&self, target: &FocusTarget) -> Result<()>;
    fn watch(&self) -> Result<ProviderWatcher>;
    fn runtime_root(&self) -> &Path;
    /// Worker updates are coalesced, just like filesystem watcher events.
    fn take_changed(&self) -> bool {
        false
    }
    fn diagnostic(&self) -> Option<ProviderDiagnostic> {
        None
    }
}
