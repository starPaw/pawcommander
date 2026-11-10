# PawCommander user guide

[← Back to the README](../README.md)

## Your workspace

Use one pane for browsing or enable **Dual pane** (`F3`) to keep two locations open. Each pane has its own tabs, history, filter, sort order, and list/grid view. Drag a tab to reorder it or move it to the other pane. Moving the last tab opens a home tab in its place.

Click a breadcrumb to navigate, or press `Cmd/Ctrl+L` to enter a path. The address editor offers folder completion. `Enter` opens the entered path; `Esc` returns to breadcrumbs. Use the sidebar for common folders and bookmarks.

Choose a language under **Settings → Language**. English is the default; Russian is also available. File names and contents are unchanged. Choose Latte, Frappé, Macchiato, or Mocha under **Settings → Catppuccin**. System appearance switches between Latte and Frappé.

Visible local folders refresh in the background approximately two seconds after the previous read finishes. Selection is retained by path; filters, sorting, and scroll offsets are preserved. Hidden panes, inactive tabs, and SFTP folders are not polled. Large or slow folders may take longer. Use `F5` to refresh manually. The selected local file's preview reloads when observed metadata changes; this is not a content watcher.

## Selection, clipboard, and drag and drop

Click a row or tile to select it, use `Cmd/Ctrl` for individual additions and `Shift` for a range, or use the selection checkboxes. File commands apply to the active pane.

Copy and cut local files to the system clipboard with `Cmd/Ctrl+C` / `X`; paste with `Cmd/Ctrl+V`. If the OS clipboard is unavailable, PawCommander reports it and falls back to its internal clipboard. SFTP sources use the internal clipboard.

- **macOS:** copy with `Cmd+C` and move at the destination with `Option+Cmd+V`, including between Finder and PawCommander. A cut inside PawCommander can also be moved by ordinary paste inside PawCommander.
- **Windows:** Explorer and PawCommander exchange local file paths and copy/move intent. Virtual items without local paths are not supported.
- **Linux:** Nautilus and Dolphin clipboard formats are supported. X11/XWayland needs an accessible clipboard; Wayland requires `ext-data-control` or `wlr-data-control`. Keep PawCommander open until the receiving app has pasted.

Drag a selection onto a folder, pane, tab, or breadcrumb. Internal drops copy by default; holding `Shift` moves local items or items within the same SFTP connection. Cross-connection and local/remote drops copy only. `Esc` cancels an internal drag.

Incoming drops from a system file manager always copy. Outgoing OS drag and drop is implemented on macOS for local files: Finder copies them, and other apps may open or attach them. Remote items must first be copied locally. Outgoing native drag on Windows/Linux and remote file promises are not implemented.

## Copying, moving, and conflicts

The operation panel shows the current item, elapsed time, and, when available, copied bytes, speed, and an estimate for the current file. One file operation runs at a time; browsing remains available.

When names collide, choose **Replace**, **Skip**, or **Keep both**. Replacement is offered for two different regular files. Keep both chooses an available numbered name; folders are copied as separate trees rather than merged. A choice can apply to later conflicts in the same operation; incompatible conflicts ask again. Cancelling the conflict dialog leaves the batch unstarted.

Copying stages data at the destination before publishing each top-level item by rename. Free-space checks run before copying; space is estimated rather than reserved. The source is checked for observable metadata changes before publication. These checks are not a snapshot or a hash comparison: concurrent changes can still escape detection, particularly with coarse SFTP timestamps. Stop writes to a source when you need a consistent copy.

If a batch fails, earlier completed items remain. PawCommander attempts to remove incomplete staging data and restore a replaced file on failure; if cleanup or restoration fails, the error identifies the remaining path. Review that path before retrying. A replacement path can briefly be absent during publication. There is no automatic resume or rollback of the whole batch. Moves across local filesystems may fail without removing the source.

Local copies preserve file contents, symlink targets, ordinary permissions, and regular-file modification times. They do not preserve all extended metadata, ACLs, Finder tags, ownership, hard-link relationships, or folder dates. This is not a complete backup or a faithful macOS app-bundle migration tool. Same-filesystem moves use rename.

Local delete sends items to the OS trash after confirmation. Restore them using the system file manager. Remote delete is permanent and uses a separate explicit confirmation.

## SFTP connections

1. Verify the server's fingerprint through a trusted channel. Connect with OpenSSH and accept only the verified host key so it is recorded in `~/.ssh/known_hosts`:

   ```sh
   ssh -p 22 user@example.com
   ```

2. Choose **SFTP** in PawCommander. Enter host, port, user, and starting folder. `.` means the server's home folder.
3. Supply an account password, a private-key file and optional passphrase, or leave password/key empty to use an SSH agent. An account password takes priority over the key.

Unknown and changed host keys are rejected. Host, port, and key path must be supplied explicitly: PawCommander does not interpret `~/.ssh/config`, ProxyJump, interactive MFA, or automatic key-file discovery. Add agent keys with `ssh-add`.

The address bar also accepts `sftp://user@example.com:22/path` with agent authentication or an existing connection. Passwords in URLs are rejected. Remote absolute and relative paths belong to the server; use a local sidebar location to return to local files.

### Saved keys and privacy

Account passwords are not saved. If you opt to save a key and passphrase after connecting, a copy is stored in the OS credential store: Keychain on macOS, Credential Manager on Windows, or Secret Service on Linux. Saving happens after server verification, authentication, and opening the starting folder succeed.

UI settings contain connection profiles and key-file paths, but not passwords or private-key contents. These profiles, tabs, and bookmarks can still reveal hostnames, usernames, and local paths; do not share settings files without reviewing them. Removing a saved secret leaves the profile, original key file, and existing sessions intact. A locked or unavailable credential store causes an error rather than a plaintext fallback.

### Transfers and recovery

Regular files and folders can be copied recursively in either direction and between connections. Rename and move are supported within one connection. Cross-connection moves are not supported; copy and verify separately. SFTP copying rejects symbolic links and special files and does not preserve source ownership, ACLs, permissions, or dates.

The server must support `fstatvfs@openssh.com` free-space reporting and provide the metadata required for source checks. Unsupported reporting stops the current item before data transfer. Destination space checks may create and remove an empty probe in a private temporary directory.

A lost connection may leave a `.pawcommander-sftp-*` staging directory; the error identifies its path. Inspect the destination and staging directory before retrying. Reconnect with the SFTP dialog. There is no transfer resume. SSH operations use a 20-second timeout; TCP connection attempts use 10 seconds per address. DNS resolution is handled by the OS.

Remote deletion recursively removes directories without a trash. Symlinks are removed without following them. An interrupted or failed deletion may already have removed part of the selection. Remote folders do not automatically refresh; use `F5` after external changes.

## Previews and properties

The details pane previews UTF-8 text and PNG, JPEG, GIF, and WebP images. Reads are bounded: text previews show the beginning of a file (up to 32 KiB), and images are limited to 20 MiB with additional dimension limits. Remote symlinks and special files are not read for preview.

Select local items and use **Properties** or `Alt+Enter` to calculate their combined logical size and counts in the background. Hidden entries are included; overlapping selected trees are counted once. Symlinks are counted separately without following targets. Hard links are counted per path. The result is logical file size, not disk usage.

Properties show available dates and permissions for selected roots. Unix permissions, UID, and GID do not represent ACLs or effective access; Windows shows read-only state. Properties are read-only and are not available for SFTP. Stop counting, close the window, or press `Esc` to request cancellation. Results can be partial when files disappear or access fails. Cancellation does not interrupt a blocked OS call.

## Search and Zed

`Cmd/Ctrl+F` filters names in the current folder. `Cmd/Ctrl+Shift+F` searches local indexed paths using `locate` or `plocate`. Enter a substring or wildcard such as `*.rs`; open a result to reveal it in its parent folder. Search is case-insensitive, limited to 1,000 results and a 15-second wait, and can be cancelled.

An index can omit restricted folders and contain stale paths. Search does not read file contents or index SFTP servers. The app does not install tools or enable indexing services. Check **Settings → Tools** to see detected tools, and recheck after installing them. On Windows, compatible command-line tools must be available in `PATH`.

**Open in Zed** sends a local path or an SSH URL to Zed. Normal opening of a remote file uses a bounded content check to recognize text (UTF-8 or BOM-marked UTF-16); explicitly choosing Zed bypasses that check. Other remote files must be copied locally before opening in external apps.

Zed establishes its own SSH connection using its SSH configuration and authentication. PawCommander's stored secrets are not passed to Zed. A server must allow full SSH access and execution of Zed's helper; SFTP-only access is insufficient. Zed may install its remote server under `~/.zed_server`. Edits are saved on the server; refresh PawCommander afterwards. See [Zed Remote Development](https://zed.dev/docs/remote-development).

## Keyboard reference

| Action | Shortcut |
| --- | --- |
| Show / hide the second pane | `F3` |
| Switch panes | `Tab` (outside text editing) |
| New / close tab | `Cmd/Ctrl+T` / `Cmd/Ctrl+W` |
| Back / forward | `Alt+Left` / `Alt+Right` |
| Parent folder | `Alt+Up` |
| Enter a path | `Cmd/Ctrl+L` |
| Filter current folder | `Cmd/Ctrl+F` |
| Search indexed local paths | `Cmd/Ctrl+Shift+F` |
| Show / hide hidden files | `Cmd/Ctrl+H` |
| Refresh | `F5` |
| Select all | `Cmd/Ctrl+A` |
| Copy / cut / paste | `Cmd/Ctrl+C` / `Cmd/Ctrl+X` / `Cmd/Ctrl+V` |
| Move on paste (macOS) | `Option+Cmd+V` |
| Rename | `F2` |
| Local properties | `Alt+Enter` |
| Create folder | `Cmd/Ctrl+Shift+N` |
| Delete with confirmation | `Delete` |
| Open selected item | `Enter` |
