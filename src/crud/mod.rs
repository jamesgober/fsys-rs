//! CRUD operations on files and directories.
//!
//! All operations are implemented as `impl Handle` blocks:
//! - `file`: file write, read, append, delete, copy, exists, size, metadata.
//! - `dir`: directory create, remove, list, exists.

pub(crate) mod dir;
pub(crate) mod file;
