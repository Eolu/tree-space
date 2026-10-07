//! `ts` binary — a thin wrapper around [`tree_space::entry::run`].
//!
//! The same entry point is exposed under two names so that `cargo install
//! tree-space` provides both `ts` (short, for interactive use) and
//! `tree-space` (used by desktop entries and packaging, and free of the
//! `/usr/bin/ts` name clash with `moreutils`).

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tree_space::entry::run()
}
