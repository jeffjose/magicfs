//! magicfs — present a directory in whatever order you want, so that `*`
//! expands to it.
//!
//! The central constraint, and the reason this tool works the way it does: the
//! shell expands `*` by reading the directory and then **sorting the names
//! itself**, in its own memory, before the command ever runs. No filesystem
//! can influence that — not ext4, not FUSE. The only lever available is the
//! names, so magicfs presents each file under an index-prefixed name chosen so
//! that alphabetical order reproduces the requested order.
//!
//! For callers that can take an ordered argument list instead
//! (`magicfs exec` / `magicfs paths`), the original filenames are preserved,
//! because argv order is not re-sorted by anything.

pub mod app;
pub mod cli;
pub mod demo;
pub mod entry;
pub mod links;
pub mod naming;
pub mod order;
pub mod shellinit;
pub mod spec;
pub mod view;

#[cfg(test)]
mod testutil;
