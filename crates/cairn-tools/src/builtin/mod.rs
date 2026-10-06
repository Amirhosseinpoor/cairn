//! The built-in tools (SPEC §6.2).

pub mod bash;
pub mod common;
pub mod edit_file;
pub mod fsio;
pub mod git;
pub mod glob;
pub mod grep;
pub mod jobs;
pub mod list_dir;
pub mod read_file;
pub mod state;
pub mod web;
pub mod write_file;

use std::sync::Arc;

pub use bash::Bash;
pub use edit_file::{EditFile, MultiEdit};
pub use git::{GitCommit, GitDiff, GitStatus};
pub use glob::Glob;
pub use grep::Grep;
pub use jobs::{BashBackground, JobKill, JobOutput};
pub use list_dir::ListDir;
pub use read_file::ReadFile;
pub use state::{AskUser, TodoWrite};
pub use web::WebFetch;
pub use write_file::WriteFile;

use crate::registry::{Registry, RegistryError};

/// Register every built-in tool this build has.
///
/// # Errors
/// [`RegistryError`] if a tool fails registration (a defect in the tool).
pub fn register_all(registry: &mut Registry) -> Result<(), RegistryError> {
    register_with(registry, WebFetch::default())
}

/// [`register_all`] with a `web_fetch` configured from `network.*`.
///
/// # Errors
/// [`RegistryError`] if a tool fails registration (a defect in the tool).
pub fn register_with(registry: &mut Registry, web: WebFetch) -> Result<(), RegistryError> {
    registry.register(Arc::new(ReadFile))?;
    registry.register(Arc::new(ListDir))?;
    registry.register(Arc::new(Glob))?;
    registry.register(Arc::new(Grep))?;
    registry.register(Arc::new(WriteFile))?;
    registry.register(Arc::new(EditFile))?;
    registry.register(Arc::new(MultiEdit))?;
    registry.register(Arc::new(Bash))?;
    registry.register(Arc::new(BashBackground))?;
    registry.register(Arc::new(JobOutput))?;
    registry.register(Arc::new(JobKill))?;
    registry.register(Arc::new(GitStatus))?;
    registry.register(Arc::new(GitDiff))?;
    registry.register(Arc::new(GitCommit))?;
    registry.register(Arc::new(TodoWrite))?;
    registry.register(Arc::new(AskUser))?;
    registry.register(Arc::new(web))?;
    Ok(())
}
