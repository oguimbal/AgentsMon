mod claude;
mod codex;
mod focus;
mod process;
pub mod vibe;

use std::path::Path;

use agentsmon_core::{AgentSnapshot, FocusTarget, ProviderUsage};
use anyhow::Result;

pub use claude::{CStatusRecord, ClaudeProvider, ProviderWatcher};
pub use codex::{CodexProvider, CodexStatusRecord};
pub use vibe::{VibeProvider, VibeStatusRecord};

pub trait AgentProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn display_name(&self) -> &'static str {
        self.id()
    }
    fn icon_png(&self) -> Option<&'static [u8]> {
        None
    }
    fn discover(&self) -> Result<Vec<AgentSnapshot>>;
    fn usage(&self) -> Result<Option<ProviderUsage>>;
    fn focus(&self, target: &FocusTarget) -> Result<()>;
    fn watch(&self) -> Result<ProviderWatcher>;
    fn runtime_root(&self) -> &Path;
}
