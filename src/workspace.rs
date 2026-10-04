//! Minimal Hyprland IPC client, used to pin the panel to a workspace and to
//! move it between workspaces.
//!
//! Hyprland exposes two Unix sockets under `$XDG_RUNTIME_DIR/hypr/<signature>/`:
//! `.socket.sock` (commands, the same protocol `hyprctl` speaks) and
//! `.socket2.sock` (a one-way event stream). tree-space uses the command socket
//! to read the active workspace, to list workspaces, and to `dispatch workspace`
//! when moving itself, and subscribes to the event socket to learn when the
//! active workspace changes so it can hide or show itself.
//!
//! None of this exists outside a Hyprland session; [`Hyprland::connect`] then
//! returns `None` and callers fall back to "always visible".

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

/// Connection details for a running Hyprland instance.
#[derive(Debug, Clone)]
pub struct Hyprland {
    /// Command socket (`.socket.sock`).
    command: PathBuf,
    /// Event socket (`.socket2.sock`).
    event: PathBuf,
}

impl Hyprland {
    /// Locate the running instance's sockets from the environment. Returns
    /// `None` when not under Hyprland, or when the sockets are not present.
    pub fn connect() -> Option<Self> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
        let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
        let base = PathBuf::from(dir).join("hypr").join(signature);
        let command = base.join(".socket.sock");
        let event = base.join(".socket2.sock");
        if !command.exists() || !event.exists() {
            return None;
        }
        Some(Self { command, event })
    }

    /// Send one command and return its response. `hyprctl`-style `j/` commands
    /// return JSON; dispatch commands return `ok`.
    fn command(&self, command: &str) -> Option<String> {
        let mut stream = UnixStream::connect(&self.command).ok()?;
        stream.write_all(command.as_bytes()).ok()?;
        stream.shutdown(std::net::Shutdown::Write).ok()?;
        let mut response = String::new();
        stream.read_to_string(&mut response).ok()?;
        Some(response)
    }

    /// The name of the workspace currently on the focused monitor.
    pub fn active_workspace(&self) -> Option<String> {
        let raw = self.command("j/activeworkspace")?;
        let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
        value.get("name")?.as_str().map(str::to_owned)
    }

    /// Existing workspace names, ordered by numeric id (so "previous"/"next"
    /// follow the compositor's ordering, not creation order).
    pub fn workspace_names(&self) -> Vec<String> {
        let raw = self.command("j/workspaces").unwrap_or_default();
        let list: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap_or_default();
        let mut named: Vec<(i64, String)> = list
            .iter()
            .filter_map(|workspace| {
                let id = workspace.get("id")?.as_i64()?;
                let name = workspace.get("name")?.as_str()?;
                Some((id, name.to_owned()))
            })
            .collect();
        named.sort_by_key(|(id, _)| *id);
        named.into_iter().map(|(_, name)| name).collect()
    }

    /// Ask the compositor to switch to `name`.
    ///
    /// The portable form is `dispatch workspace <name>`, but a Lua-configured
    /// Hyprland (Omarchy) routes `dispatch` through `hl.dispatch` and rejects
    /// it; there the `hl.dsp.focus` dispatcher is required. Try the portable
    /// form first and fall back when the compositor reports an error.
    pub fn dispatch_workspace(&self, name: &str) {
        let classic = self.command(&format!("dispatch workspace {name}"));
        if classic.as_deref().is_some_and(|response| response.trim() == "ok") {
            return;
        }
        let _ = self.command(&format!("dispatch hl.dsp.focus({{ workspace = \"{name}\" }})"));
    }

    /// Spawn a thread that calls `on_change` with the newly focused workspace's
    /// name whenever it changes. The thread exits when the socket closes (e.g.
    /// the compositor restarts).
    pub fn spawn_watcher(&self, on_change: impl Fn(String) + Send + 'static) {
        let path = self.event.clone();
        std::thread::spawn(move || {
            let Ok(stream) = UnixStream::connect(&path) else { return };
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                // `workspace>>NAME` on a workspace switch, and
                // `focusedmon>>MON,NAME` when focus moves between monitors.
                let workspace = line
                    .strip_prefix("workspace>>")
                    .or_else(|| line.strip_prefix("focusedmon>>").and_then(|rest| rest.split_once(',').map(|(_, ws)| ws)));
                if let Some(name) = workspace {
                    on_change(name.to_owned());
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_returns_none_without_hyprland_env() {
        // This test process is not a Hyprland instance (no signature), so
        // connecting must fail cleanly rather than panic.
        // Guard: if the test *is* run inside Hyprland, skip the assertion.
        if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
            assert!(Hyprland::connect().is_none());
        }
    }
}
