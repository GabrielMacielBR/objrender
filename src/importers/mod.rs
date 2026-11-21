//! Isolated format importers translating external spatial formats into generic `Scene`s.
//!
//! Nothing inside the rendering core may depend on this module.

pub mod damf;

use thiserror::Error;

use crate::scene::{Scene, SceneError};

#[derive(Debug, Error)]
pub enum ImportError {
    #[error("scene construction error: {0}")]
    Scene(#[from] SceneError),
    #[error("invalid importer descriptor: {0}")]
    InvalidDescriptor(String),
}

/// Format adapter boundary: converts an external format input into a generic [`Scene`].
pub trait SceneImporter {
    fn import(&self) -> Result<Scene, ImportError>;
}
