//! DiskLens scan engine.
//!
//! [`scan::Scanner`] walks a folder with a pool of worker threads and streams one
//! [`scan::DirResult`] per folder over a channel. The owner of the [`tree::Tree`]
//! (the CLI loop, or the GTK main loop) applies them; each applied folder adds its
//! own files to every ancestor, so totals are always consistent with what has been
//! applied so far. No locks are shared between the tree and the workers.

pub mod dups;
pub mod scan;
pub mod stats;
pub mod trash;
pub mod tree;
pub mod util;

pub use scan::{Msg, ScanOptions, Scanner};
pub use tree::{Dir, FileEntry, Tree, NONE};
