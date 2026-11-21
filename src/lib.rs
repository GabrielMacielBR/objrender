#![forbid(unsafe_code)]

//! Offline 3D object-based spatial audio renderer for arbitrary speaker layouts.

pub mod importers;
pub mod io;
pub mod layout;
pub mod math;
pub mod mixer;
pub mod renderer;
pub mod scene;
pub mod timeline;
