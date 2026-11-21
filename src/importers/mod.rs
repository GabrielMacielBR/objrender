//! Isolated format importers translating external spatial formats into generic `Scene`s.
//!
//! Nothing inside the rendering core may depend on this module.

pub mod caf;
pub mod damf;

use thiserror::Error;

use crate::io::IoError;
use crate::scene::{Scene, SceneError};

#[derive(Debug, Error)]
pub enum ImportError {
    #[error("scene construction error: {0}")]
    Scene(#[from] SceneError),
    #[error("I/O error: {0}")]
    Io(#[from] IoError),
    #[error("filesystem error reading `{path}`: {source}")]
    Fs {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("YAML parse error: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),
    #[error("invalid importer descriptor: {0}")]
    InvalidDescriptor(String),
}

/// Format adapter boundary: converts an external format input into a generic [`Scene`].
pub trait SceneImporter {
    fn import(&self) -> Result<Scene, ImportError>;
}
