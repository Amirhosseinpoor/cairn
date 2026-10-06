//! The built-in tools (SPEC §6.2).

pub mod common;
pub mod edit_file;
pub mod fsio;
pub mod glob;
pub mod grep;
pub mod list_dir;
pub mod read_file;
pub mod write_file;

use std::sync::Arc;

pub use edit_file::{EditFile, MultiEdit};
pub use glob::Glob;
pub use grep::Grep;
pub use list_dir::ListDir;
pub use read_file::ReadFile;
pub use write_file::WriteFile;

use crate::registry::{Registry, RegistryError};

/// Register every built-in tool this build has.
///
/// # Errors
/// [`RegistryError`] if a tool fails registration (a defect in the tool).
pub fn register_all(registry: &mut Registry) -> Result<(), RegistryError> {
    registry.register(Arc::new(ReadFile))?;
    registry.register(Arc::new(ListDir))?;
    registry.register(Arc::new(Glob))?;
    registry.register(Arc::new(Grep))?;
    registry.register(Arc::new(WriteFile))?;
    registry.register(Arc::new(EditFile))?;
    registry.register(Arc::new(MultiEdit))?;
    Ok(())
}
