# Contributing

Thanks for your interest in tree-space.

## Development

The project is developed against Hyprland on Wayland; it is not yet tested on
other compositors, and reports or fixes for those are welcome. It is
Wayland-only by design — there is no X11 support.

See [Build dependencies](README.md#build-dependencies) for the system packages
needed to build.

```bash
cargo build            # debug build (both `ts` and `tree-space`)
cargo test             # headless unit tests (no display needed)
cargo clippy --all-targets -- -D warnings
cargo fmt
```

The non-visual logic (config, the tree model, file operations, previews) is
covered by `cargo test` and needs no display. Please keep it that way: put new
logic in `src/fs`, `src/config`, `src/preview` or `src/cmd` behind a trait so it
can be tested without GTK.

## Pull requests

- Keep the change focused and add tests where it makes sense.
- Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test`
  before opening the PR.
- Match the existing style; comments explain *why*, not *what*.

## Releases

Releases are cut from the public repository
[`Eolu/tree-space`](https://github.com/Eolu/tree-space) (this repo is mirrored
there). Packaging lives in `packaging/`; see `packaging/aur`.

## Reporting bugs

Feature parity with a full GUI file manager is the goal, so "a file manager
should do X" is exactly the kind of issue worth filing. Please include your
compositor, distribution, and the tree-space version (`tree-space --version`).
