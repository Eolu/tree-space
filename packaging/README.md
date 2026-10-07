# Packaging

Distribution packaging for tree-space.

## Arch / Omarchy (AUR)

Two packages live under `packaging/aur/`:

| Directory | Package | Source |
| --- | --- | --- |
| `tree-space/` | `tree-space` | the tagged release tarball from GitHub |
| `tree-space-git/` | `tree-space-git` | the latest `master` |

Both install `/usr/bin/tree-space` (not `ts`, which belongs to `moreutils`), the
desktop entry, and the icon, and ship the `FileManager1` D-Bus service dormant
under `/usr/share/tree-space/`.

### Updating the stable package

1. Publish the release tag on GitHub (`vX.Y.Z`).
2. Refresh the checksum — it is `SKIP` in the repository because the tag may not
   exist yet:

   ```bash
   cd packaging/aur/tree-space
   updpkgsums
   ```

3. Regenerate `.SRCINFO`:

   ```bash
   makepkg --printsrcinfo > .SRCINFO
   ```

4. Build it locally to be sure:

   ```bash
   makepkg -f
   ```

### Submitting to the AUR

The AUR keeps each package in its own git repository. For the stable package:

```bash
git clone ssh://aur@aur.archlinux.org/tree-space.git
cp packaging/aur/tree-space/PKGBUILD \
   packaging/aur/tree-space/tree-space.install \
   packaging/aur/tree-space/.SRCINFO tree-space/
cd tree-space && git add . && git commit -m 'tree-space X.Y.Z' && git push
```

Repeat for `tree-space-git`. Install with `yay -S tree-space` (or
`tree-space-git`).

A `tree-space-bin` package is not provided yet: it needs prebuilt release
artifacts, which require a GitHub release/CI workflow that is not set up.

## Omarchy

Omarchy is Arch-based, so the AUR packages above work as-is. tree-space already
picks up the active Omarchy theme with `[theme] mode = "system"` (see the main
README), so no extra integration is required. To autostart the panel, add a
Hyprland `exec-once` for `tree-space --hidden` and a bind to toggle it, for
example in `~/.config/hypr/`.

## Other distributions

crates.io (`cargo install tree-space`) is the distribution-independent channel;
see the main README for the system build dependencies. Debian/Fedora/Nix
packaging is welcome but not provided here yet.
