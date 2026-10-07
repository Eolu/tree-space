//! `tree-space` binary — a thin wrapper around [`tree_space::entry::run`].
//!
//! Identical to the `ts` binary. Packaging installs this name (`/usr/bin/
//! tree-space`) because `/usr/bin/ts` is already owned by `moreutils` on Arch;
//! `cargo install tree-space` still provides both.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tree_space::entry::run()
}
