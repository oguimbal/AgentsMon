mod claude;
mod codex;
mod focus;

use std::path::Path;

use agentsmon_core::{AgentSnapshot, FocusTarget, ProviderUsage};
use anyhow::Result;

pub use claude::{CStatusRecord, ClaudeProvider, ProviderWatcher};
pub use codex::{CodexProvider, CodexStatusRecord};

pub trait AgentProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn discover(&self) -> Result<Vec<AgentSnapshot>>;
    fn usage(&self) -> Result<Option<ProviderUsage>>;
    fn focus(&self, target: &FocusTarget) -> Result<()>;
    fn watch(&self) -> Result<ProviderWatcher>;
    fn runtime_root(&self) -> &Path;
}
