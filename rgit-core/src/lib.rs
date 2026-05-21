//! Core data structures and operations for rgit.
//!
//! Organized by Git's natural component boundaries. The [`object`] module
//! owns the in-memory representation, parsing, and serialization of Git's
//! four object kinds (blob, tree, commit, tag) and the [`ObjectId`] type
//! that names them. Higher-layer modules (object database, pack, refs,
//! index, working tree, transport) build on top and will be added
//! incrementally.

pub mod diff;
pub mod gitignore;
pub mod index;
pub mod merge;
pub mod object;
pub mod odb;
pub mod pack;
pub mod refs;
pub mod transport;
pub mod workdir;

pub use object::{Object, ObjectId, ObjectKind};
pub use odb::Repository;
pub use refs::{HeadState, RefError};
