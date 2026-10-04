//! Freedesktop desktop-integration glue.
//!
//! Most "open this folder" requests go through the XDG MIME default for
//! `inode/directory` (`resources/tree-space.desktop` registers there), which is
//! why `xdg-open <dir>` and compositor keybinds honour tree-space. A few
//! integrations bypass the MIME database and ask the desktop's file manager
//! over D-Bus instead:
//!
//! * xdg-desktop-portal's `org.freedesktop.portal.OpenURI.OpenDirectory` — used
//!   by Electron apps (e.g. VS Code's "Open Containing Folder") and the file
//!   chooser's "Show in folder" — is documented to call
//!   `org.freedesktop.FileManager1.ShowItems`, falling back to the MIME default
//!   only when nothing provides that service.
//! * Nautilus ships
//!   `/usr/share/dbus-1/services/org.freedesktop.FileManager1.service`, which
//!   names itself, so without this module that call activates Nautilus.
//!
//! The running panel claims `org.freedesktop.FileManager1` on the session bus
//! and answers the three methods by forwarding to the instance socket exactly
//! like `ts --select <path>`. `g_bus_own_name` also re-acquires the name if it
//! is momentarily owned by another file manager that later exits.

use relm4::gtk::gio;
use relm4::gtk::glib;

use crate::cmd::Command;
use crate::ipc;

/// The well-known name desktop integrations call.
pub const BUS_NAME: &str = "org.freedesktop.FileManager1";
/// The object path the FileManager1 methods live on.
const OBJECT_PATH: &str = "/org/freedesktop/FileManager1";

/// Introspection data for the interface. All three methods share the same
/// `(as URIs, s StartupId)` signature.
const INTERFACE_XML: &str = r#"<node>
  <interface name="org.freedesktop.FileManager1">
    <method name="ShowFolders">
      <arg type="as" name="URIs" direction="in"/>
      <arg type="s" name="StartupId" direction="in"/>
    </method>
    <method name="ShowItems">
      <arg type="as" name="URIs" direction="in"/>
      <arg type="s" name="StartupId" direction="in"/>
    </method>
    <method name="ShowItemProperties">
      <arg type="as" name="URIs" direction="in"/>
      <arg type="s" name="StartupId" direction="in"/>
    </method>
  </interface>
</node>"#;

/// Claim [`BUS_NAME`] on the session bus and serve the FileManager1 interface.
///
/// Best-effort: if another owner already holds the name the claim is deferred
/// (GLib keeps retrying and takes over when it is freed), and a missing session
/// bus is a no-op. Must run on the thread that iterates the main context, before
/// the loop starts — it is safe to call from `main` just before `RelmApp::run`.
pub fn serve_file_manager() {
    gio::bus_own_name(
        gio::BusType::Session,
        BUS_NAME,
        gio::BusNameOwnerFlags::empty(),
        |connection, _name| register_object(&connection),
        |_connection, _name| {},
        |_connection, _name| {},
    );
}

/// Export the FileManager1 object on an acquired bus connection.
fn register_object(connection: &gio::DBusConnection) {
    let Ok(node) = gio::DBusNodeInfo::for_xml(INTERFACE_XML) else {
        return;
    };
    let Some(interface) = node.lookup_interface(BUS_NAME) else {
        return;
    };
    let _ = connection
        .register_object(OBJECT_PATH, &interface)
        .method_call(|_conn, _sender, _path, _iface, _method, params, invocation| {
            // Every method has the signature `(as, s)`; the startup-id is only
            // used for focus/activation stealing, which a docked panel has no
            // need for, so all three are treated as a reveal.
            let uris: Vec<String> =
                params.get::<(Vec<String>, String)>().map(|(uris, _)| uris).unwrap_or_default();
            let command = command_for_uris(&uris);
            if !command.reveal.is_empty() {
                let _ = ipc::deliver(&command);
            }
            invocation.return_value(None);
        })
        .build();
}

/// Turn a FileManager1 request's URI list into the reveal command the instance
/// socket speaks. Non-`file:` URIs are ignored.
fn command_for_uris(uris: &[String]) -> Command {
    let mut command = Command::default();
    for uri in uris {
        if let Ok((path, _hostname)) = glib::filename_from_uri(uri) {
            command.reveal.push(path);
        }
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn file_uris_become_decoded_reveals() {
        let uris = vec!["file:///home/eolu/My%20Files/notes.md".to_owned()];
        assert_eq!(
            command_for_uris(&uris).reveal,
            vec![PathBuf::from("/home/eolu/My Files/notes.md")]
        );
    }

    #[test]
    fn non_file_uris_are_ignored() {
        let uris = vec!["https://example.com/".to_owned(), "trash:///x".to_owned()];
        assert!(command_for_uris(&uris).reveal.is_empty());
    }

    #[test]
    fn multiple_uris_all_reveal() {
        let uris =
            vec!["file:///tmp/a".to_owned(), "file:///tmp/b%20c".to_owned()];
        assert_eq!(
            command_for_uris(&uris).reveal,
            vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b c")]
        );
    }
}
