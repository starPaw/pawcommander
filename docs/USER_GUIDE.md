# PawCommander user guide

[← Back to the README](../README.md)

## Your workspace

Use one pane for browsing or enable **Dual pane** (`F3`) to keep two locations open. Each pane has its own tabs, history, filter, sort order, and list/grid view. Drag a tab to reorder it or move it to the other pane. Moving the last tab opens a home tab in its place.

In **Settings**, choose **Side by side** or **Top and bottom** under **Pane layout**. Drag the divider to resize the panes. The **Sidebar** button on the toolbar hides or shows the sidebar. PawCommander remembers the layout, the divider position for each orientation, and sidebar visibility.

PawCommander remembers view changes for each folder. Use the pane controls to choose List or Grid and change sorting, and use **Settings** to set **Density** and show hidden files. Returning to a folder restores its saved choices, even from the other pane.

Open **Settings → List columns** to show or hide columns, set widths, and reorder optional columns. Name stays first. The list scrolls horizontally when the configured columns exceed the pane width. Permissions and numeric owner IDs are available for Unix and SFTP entries that supply them. Windows local entries show a dash for those fields. SFTP creation dates also appear as a dash.

Open **Folder tree** in the sidebar to browse the active pane's filesystem or connected SFTP server. The tree highlights the current folder and reads subfolders as branches expand. Select a folder name to open it in the active pane. Use **Refresh** on the toolbar to reload expanded branches.

Choose **Column view** in the pane controls to browse the current folder alongside up to two of its ancestors. Use Up and Down to select a row, Right to enter a selected folder, and Left to go to its parent. Select a sibling folder in an ancestor column to jump to it. Scroll horizontally when the columns exceed the pane width.

Click a breadcrumb to navigate, or press `Cmd/Ctrl+L` to enter a path. The address editor offers folder completion. `Enter` opens the entered path; `Esc` returns to breadcrumbs. Use the sidebar for common folders and bookmarks.

Choose a language under **Settings → Language**. PawCommander defaults to English and saves your selection. On macOS you can choose **English**, **Русский**, or **Čeština**. File lists and properties use the selected language for dates, numbers, and sizes. Enter search filter dates as `YYYY-MM-DD`.

Choose Latte, Frappé, Macchiato, or Mocha under **Settings → Catppuccin**. System appearance switches between Latte and Frappé.

Opening a local or SFTP folder shows entries progressively while **Reading folder…** remains visible. File metadata is read in the background. Navigating away, refreshing, or closing the tab cancels the previous read between filesystem or SSH calls; an already blocked call must finish or time out first. If a read fails after showing entries, the partial listing stays visible with an error. Refreshing a previously complete listing replaces it as a whole. Large folders reuse the current filtered and sorted view between updates. The loaded listing stays in memory, so usage grows with the number of entries and the length of their names.

Visible folders refresh in the background after the previous read finishes: approximately every two seconds locally and every ten seconds for SFTP. Selection is retained by path; filters, sorting, and scroll offsets are preserved. Hidden panes and inactive tabs are not polled. SFTP polling uses an existing authenticated session and skips a busy refresh or unavailable session without clearing the listing or prompting for credentials. Automatic refresh pauses during file operations and dragging. Large or slow folders may take longer. Use `F5` to refresh manually. The selected local file's preview reloads when observed metadata changes; this is not a content watcher.

## Launch from Terminal on macOS

Run the `pawcommander` executable from the app bundle at `/Applications/PawCommander.app/Contents/MacOS/pawcommander` or from your source build. Use the examples below if the executable is on your `PATH`.

```sh
pawcommander --help
pawcommander open "$PWD"
pawcommander workspace "My project"
pawcommander search '*.rs' "$PWD"
```

`workspace` takes the saved name of a folder pair. `search` opens a file name or mask search in the given folder or, by default, the shell's current directory. Quote names and paths containing spaces, and put `--` before operands that start with a dash. A lone folder path is also accepted, and a launch with zero arguments restores the saved session.

The `open`, `workspace` and `search` commands create a window with a temporary copy of saved settings. Changes to its settings and saved workspaces last for that window. The terminal process waits until the window closes.

Exit status `0` means help displayed or the window closed normally. `1` reports settings or startup errors, `2` reports invalid arguments, and `3` reports an unavailable folder or workspace.

## ZIP and TAR archives

Double-click a local `.zip` or uncompressed `.tar` file, or select it and press `Enter`. PawCommander opens the archive in the current pane and reads its directory structure and metadata. **Open** in the search results pane also opens the archive in the active pane. Browse nested folders from the file list. From the archive root, use **Parent folder** to return to the folder containing the archive. Use **Back** and **Forward** to revisit locations, and **Refresh** to reread an archive after it changes.

Entries stay inside the archive while you browse. PawCommander reports unreadable archives and invalid member paths in the pane. The browser indexes up to 50,000 items, including implied folders, and supports up to 64 path levels.

Select files or folders and choose **Create archive…** from the context menu or **Create archive** from the command palette. Choose ZIP (Deflate) or TAR (uncompressed) and enter a name. The queue displays progress and lets you **Stop** the operation. The completed archive appears in the current folder, including on SFTP. Choose an unused name to keep existing files. PawCommander archives regular files and folders, including empty folders. Symbolic links and special files require a different selection.

Enter an optional password when creating a ZIP to encrypt file contents with AES-256. Entry names remain visible when browsing. Leave the password field empty to create a plain ZIP.

Inside an archive, select the entries you need and choose **Extract…** from the context menu or **Extract selected entries** from the command palette. Enter a destination or use the folder in the other pane, including an SFTP connection. For name conflicts, choose **Skip**, **Keep both**, or **Replace** for two different regular files. Folders publish as a whole after their files have been written, and existing folders can be kept under separate names. **Stop** removes the active partial output and keeps entries that already finished.

Extraction supports ZIP (Stored or Deflate) and uncompressed TAR with regular files and directories. For protected ZIP files, enter the password in the extract dialog. PawCommander reads WinZip AES and legacy ZipCrypto entries. It checks recorded sizes and authenticates AES entries before publishing them. An incorrect password leaves the affected output unpublished. Select up to 32 GiB of uncompressed data per operation. Links and special entries need a different selection. The application keeps the password in memory for the operation.

Choose **Check archive…** on a single local archive or **Check archive** from the command palette while browsing one. The queue checks the whole archive without extracting any files and lets you **Stop**. Open **Result history** for the names and reasons behind damaged or unverified entries. ZIP checks include recorded sizes, CRCs and supported encryption when you supply the password. TAR checks include header checksums and whether the entry data can be read. The TAR format has no per-file data checksum.

To edit a local ZIP or TAR, open it in a pane. Select local or SFTP files or folders in the other pane, then activate the archive pane and choose **Add from other pane…** or **Add to archive** in the command palette. The files are added to the archive folder shown in the pane. Use **Rename…** or **F2** on a single entry, or select entries and confirm **Remove from archive…**. Renaming or removing a folder includes its contents. Choosing an occupied name leaves the archive unchanged.

Edits rebuild and check a temporary archive before replacing the original. **Stop** discards unfinished work. Retained ZIP entries keep their compressed and encrypted data. When adding files to a protected ZIP, enter a password for the new files. Archives with ambiguous duplicate paths, unsupported retained ZIP methods, or sparse TAR records cannot be rebuilt safely by this editor.

## Selection, clipboard, and drag and drop

Click a row or tile to select it, use `Cmd/Ctrl` for individual additions and `Shift` for a range, or use the selection checkboxes. File commands apply to the active pane.

Use the command palette to select by mask or extension, invert the selection, or restore the previous selection. A mask matches the whole name and supports `*` for any number of characters and `?` for one character. Extension matching uses the final extension. Matching ignores case. These commands work on visible entries in the active tab. You can restore the previous selection once after applying a rule, inverting, selecting all, or clearing with Esc. Restore uses available metadata to keep matching entries in the current folder. On macOS it also checks the filesystem identifier of regular local files.

Copy and cut local files to the system clipboard with `Cmd/Ctrl+C` / `X`; paste with `Cmd/Ctrl+V`. If the OS clipboard is unavailable, PawCommander reports it and falls back to its internal clipboard. SFTP sources use the internal clipboard.

- **macOS:** copy with `Cmd+C` and move at the destination with `Option+Cmd+V`, including between Finder and PawCommander. A cut inside PawCommander can also be moved by ordinary paste inside PawCommander.
- **Windows:** Explorer and PawCommander exchange local file paths and copy/move intent. Virtual items without local paths are not supported.
- **Linux:** Nautilus and Dolphin clipboard formats are supported. X11/XWayland needs an accessible clipboard; Wayland requires `ext-data-control` or `wlr-data-control`. Keep PawCommander open until the receiving app has pasted.

On macOS, file paste starts when `V` is pressed; you can release `Cmd` and `Option` in any order. Text fields keep the usual `Cmd+C`, `Cmd+X`, and `Cmd+V` behavior.

Drag a selection onto a folder, pane, tab, or breadcrumb. Internal drops copy by default; holding `Shift` moves local items or items within the same SFTP connection. Cross-connection and local/remote drops copy only. `Esc` cancels an internal drag.

Incoming drops from a system file manager always copy. To drag local files into another application, drag the selection across the window edge. Outgoing native drags offer copy on macOS, Windows, Linux X11 and Wayland; the receiving app may copy, open or attach the files. Shift does not authorize an external move. Press Esc to cancel. Returning an outgoing Windows/Linux drag to PawCommander cancels that handoff; start a new internal drag to transfer between panes. The receiving app handles its own conflicts and copy progress for local items.

On macOS, drag SFTP files and folders to Finder or another app that accepts file promises. PawCommander starts downloading when the destination accepts the drop and requests the item. Use the operation panel to pause or stop the download and check **Result history** for errors. PawCommander leaves existing files in the destination in place and publishes each new item when its download is complete. Recoverable downloads appear in **Settings → Crash recovery**.

Wayland outgoing drag uses the window's active pointer press and requires `wl_data_device_manager` version 3. It does not require the clipboard's data-control extension. Wayland clipboard copy/cut/paste separately needs `ext-data-control` or `wlr-data-control` as described above.

## Batch rename

Select at least two files or folders in one local or SFTP directory and press `F2`. Edit the template and choose **Preview names** to compare the current names with the proposed ones. **Rename** becomes available when the preview contains a change and all proposed names are valid and free.

Use `{stem}` for the base name, `{ext}` for the extension including its dot, and `{n}` for a counter in the displayed selection order. Set the starting number and minimum digits for zero padding. For example, `photo-{n}{ext}` with three digits produces `photo-001.jpg`. The optional regular expression applies to the base name before the template. Use replacement groups such as `$1`, and double braces to include literal braces. Batch rename uses UTF-8 names. For folders, `{stem}` is the entire name.

Existing names, including names held by other items in the selection, and colliding proposals block the batch before it starts. **Stop** or `Shift+Esc` stops between changes. After a stop or partial failure, **Result history** shows the confirmed renames and the remaining items. Use **Undo changes** in the same session to restore confirmed renames when their original names are still free and the renamed items can be verified.

## Copying, moving, and conflicts

The operation panel shows the current item, elapsed time, and, when available, copied bytes, speed, and an estimate for the current file. One file operation runs at a time; browsing remains available. Further copies, moves, folder creation, renames, and deletions join a FIFO queue with room for 100 pending jobs. Expand **Queued** in the footer to inspect the order, move a pending job earlier or later, or **Remove from queue** before it starts. Removing a pending job makes no filesystem changes and creates no result-history record.

Choose **Pause** during a local or SFTP copy or move. The footer shows **Pausing transfer…** until the worker reaches a safe point, then **Transfer paused** with the current item and byte count. An in-flight I/O call or final publication can finish first. **Resume** continues from the current position within the same session. **Stop** or `Shift+Esc` also works while paused and cleans up the unfinished temporary copy. The paused transfer keeps its place at the front of the queue.

Open **Settings → Transfer speed…** or choose **Speed…** in the active operation panel. Enter a global limit and optional limits for individual SFTP connections in KiB/s. Use `0` for unlimited. **Save** updates the current transfer from its next block and applies to later transfers. The worker uses the lowest applicable limit. SFTP limits apply to the server, port and user. Copying within one SFTP connection uses its budget in both directions. A remote copy verification read uses the destination's limit and the global limit.

Jobs keep the source paths, destination, and action captured when submitted. Changing tabs or clipboard contents does not retarget them. Paths and conflicts are checked when each job starts, so a preceding job can create a destination or cause a later source to disappear. Queued work is not a snapshot or a reservation against other applications. The global concurrency limit of one also serializes jobs using the same destination, nested folders, or alternate path spellings. Dragging files to another application remains unavailable during file operations.

After success, failure, or a completed Stop, the next pending job starts automatically. The preceding result stays visible until another result or notice replaces it; completed file-operation results remain in **Result history**. Remove pending jobs to prevent them from starting. Pending jobs belong to the current app session and are not restored after a crash or forced termination. Normal window closing waits for the active job and queue to finish; remove pending jobs and stop the active transfer if needed.

If the operating system cannot start a worker, the queue pauses with all pending jobs retained. Choose **Retry queue** after resolving the error, or remove pending jobs. A worker-start failure does not create a completed-operation history entry.

When names collide, choose **Replace**, **Skip**, or **Keep both**. Replacement is offered for two different regular files. Keep both chooses an available numbered name; folders are copied as separate trees rather than merged. A choice can apply to later conflicts in the same operation; incompatible conflicts ask again. Cancelling the conflict dialog leaves the batch unstarted.

Use **Stop** in the operation footer or press `Shift+Esc` to stop the current copy or move. The button changes to **Stopping…** while the worker finishes its current filesystem or network call and cleans up unpublished data. You can keep browsing and queue more work. Stop affects only the active transfer; the next job waits for its cleanup and final result. Window closing remains blocked while work remains. `Shift+Esc` also works while a conflict dialog is open; ordinary `Esc` or **Cancel operation** cancels the active unstarted batch.

A stopped transfer reports completed, skipped, and remaining top-level items, with the full remaining source paths in a scrollable, selectable notice. An interrupted folder remains one pending item even if some of its children were copied privately. Cancellation during preparation lists the selected paths as unchanged without claiming an exact normalized item count. Completed items stay at the destination. Stopping is cooperative and cannot interrupt an already blocked system or SFTP call; publication, restoration and cleanup finish before the result appears. A stop request that arrives after the final item was published can still report normal completion.

Copying stages data at the destination before publishing each top-level item by rename. Free-space checks run before copying; space is estimated rather than reserved. The source is checked for observable metadata changes before publication. These checks are not a snapshot: concurrent changes can still escape detection, particularly with coarse SFTP timestamps. Stop writes to a source when you need a consistent copy.

Enable **Settings → Verify copies (SHA-256)** for an optional content check, off by default. Each newly submitted copy captures the setting, including queued copies, clipboard paste, and incoming drops. PawCommander hashes the source bytes while copying each regular file, then independently reads back its staged copy before publication. The footer shows **SHA-256 verification** and bytes read separately from bytes written. This adds a full destination read, including SFTP traffic for remote destinations. A mismatch or verification read error fails the selected item; Stop during verification cancels it. Its source and existing destination remain intact, and unpublished staging is cleaned up or reported for recovery. Earlier items already published remain completed in result history. Local symlinks are copied as links and have no file-content hash. Verification checks the bytes read during the copy, not a filesystem snapshot or future durability.

Moves within one local filesystem or SFTP connection use rename. Moves between local volumes, between local and SFTP locations, or between different connections use a verified copy followed by source removal. SHA-256 verification is mandatory for these moves, even when the copy setting is off. After publication, the source is checked again and moved into a private folder beside its original location. Files are compared with the published copy before individual removal; directories are removed only when empty. Local symlinks are compared without following them. Special objects and SFTP symlinks are refused. Windows reparse points and volumes without usable identity may remain unsupported; a failed native rename is never treated as permission to delete. Moves between filesystems within a single SFTP connection still depend on the server's rename support.

**Проверка копий:** включите **Настройки → Проверять копии (SHA-256)**; по умолчанию проверка выключена. Настройка запоминается при добавлении каждого задания копирования в очередь. Для обычного файла вычисляется хеш прочитанных исходных байтов, затем временная копия независимо читается и проверяется до публикации. Чтение для проверки отображается отдельно от записанных байтов и добавляет трафик при назначении SFTP. Несовпадение или ошибка чтения означает сбой, а остановка во время проверки — отмену: источник и прежнее назначение сохраняются, временная копия удаляется либо показывается путь восстановления. Уже опубликованные объекты остаются завершёнными в журнале. Локальные символические ссылки копируются как ссылки без хеша содержимого. Это не снимок файловой системы и не гарантия будущей сохранности. Перенос внутри одной файловой системы использует переименование. Между локальными дисками, локальной папкой и SFTP либо разными соединениями проверка копии обязательна независимо от настройки. Исходник удаляется поэлементно только после публикации и повторной сверки; при ошибке остаток сохраняется, а журнал показывает «Копия создана, перенос не завершён».

If a batch fails, earlier completed items remain. PawCommander attempts to remove incomplete staging data and restore a replaced file on failure; if cleanup or restoration fails, the error identifies the remaining path. Review that path before retrying. A replacement path can briefly be absent during publication. There is no automatic resume or rollback of the whole batch. For a copy-and-delete move, an error or Stop after publication leaves the verified destination in place. The remaining source is restored to its original name when possible. If that name is occupied or restoration fails, the message names the private folder retaining the remainder. Inspect both locations before continuing; neither automatic Retry nor rename-only Undo is offered for this partial result. Only fully removed roots consume the cut selection or count as completed moves.

Local copies preserve file contents, symlink targets, ordinary permissions, and regular-file modification times. They do not preserve all extended metadata, ACLs, Finder tags, ownership, hard-link relationships, or folder dates. This is not a complete backup or a faithful macOS app-bundle migration tool. Same-filesystem moves use rename.

Local delete sends items to the OS trash after confirmation. Restore them using the system file manager. Remote delete is permanent and uses a separate explicit confirmation.

### Crash recovery

Before a job starts, PawCommander saves a small description in `PawCommander/recovery` under the local application-data directory. If this record cannot be saved, the job does not run. After a process crash or forced termination, the next launch reports jobs without confirmed completion. Open **Settings → Crash recovery** to view that startup report again. Close the report before continuing; any dialog already open remains pending.

A report does not prove that every source is unchanged or that nothing reached the destination. Inspect both folders and **Result history** before repeating an operation. Live jobs in another running instance are omitted. Normal completion clears the marker after the result-history save is acknowledged; a caught worker panic or failed history save retains it. Corrupt or unreadable records are reported and preserved.

The report lists temporary-copy locations recorded by the interrupted job. A **planned temporary folder** was recorded before creation and may not exist or may contain someone else's data; a **registered temporary copy** was acknowledged as created. Local receipts retain the directory's identity. Remote receipts identify the recorded path, not the current remote object. The report does not scan arbitrary temporary folders, verify their contents, restore queued jobs, retry work, or remove data. Markers remain available for manual review and can appear again on later launches. Recovery after power loss is not guaranteed.

### Result history

Choose **Result history** in the footer to review completed copy, move, rename, create-folder, and delete operations, including earlier sessions. Expand an operation for source and destination paths, each selected item's outcome, and its failure or warning reason. Choose **Details** beside a row to open the complete paths and reason in a separate, scrollable window. Text can be selected and copied. Embedded line breaks appear as `\n` and `\r` in the list; details and the saved file retain the original text. **Completed with warning** means the item was published but a later step, such as temporary-folder cleanup, failed. Check the recorded recovery guidance before retrying. **Copy created, move incomplete** means a copy was published but source removal did not finish; descendant rows distinguish removed entries from remaining ones. **Publication unconfirmed** includes a lost SFTP publication reply; inspect both locations before repeating the operation.

Folder copies include the files, links, and directories from the source manifest, with their final destination paths. A nested item becomes **Completed** only after the whole selected folder is published. If a later child or final source check fails, previously prepared private copies show **Publication unconfirmed**; this also covers a lost SFTP publication reply. Inspect the selected folder's error and recovery paths before retrying. The failed or stopped child has its own outcome, and **Not started** distinguishes children not yet processed. A cleanup warning after acknowledged publication leaves the nested copies completed.

Permanent SFTP deletion records each file, link, or directory actually visited. A confirmed deletion stays **Completed** if a later sibling or parent fails; failed entries have their own reasons. An interrupted traversal shows **Descendants were not fully listed** on the selected root. Unvisited children are absent, so missing rows do not prove those files were checked or left unchanged.

If source-manifest validation fails or is stopped, copy history retains only the entries already visited by that validation. The failed or stopped entry has its own outcome and reason; entries checked before copying remain **Not started**. No private copy has been published at this stage. Unvisited descendants are absent and the selected folder shows **Descendants were not fully listed**.

Rename-based folder moves publish the whole folder with an atomic rename. The journal records observed descendants and checks them at the destination before marking their individual results **Completed**. If enumeration is unavailable or exceeds its bounds, the move can finish with a warning and an explicitly incomplete list. A descendant whose destination cannot be confirmed remains **Publication unconfirmed**.

System Trash preserves the selected folder as a unit. Its root is **Completed** when the operating system acknowledges the operation. Observed descendants have separate **Publication unconfirmed** rows explaining that the root reached Trash but the OS supplied no individual confirmation. Restore the folder using the system Trash.

Skipped and unstarted folders can show **Descendants were not fully listed**. When a local conflict is skipped, explicitly selected paths already grouped under that item also appear as **Skipped**, with a reason that no separate transfer started. These rows preserve the selected spelling, including parent aliases; they have no independently resolved target path. Unselected descendants are not enumerated.

History is saved in `PawCommander/operation-history.json` under the OS local application-data directory; its full path appears in the history window. It retains up to 100 recent operations and 100,000 rows within a 32 MiB storage budget, dropping older operations when needed. Collection for one operation also has a conservative 16 MiB budget; individual paths and messages are limited to 64 KiB. Records explicitly show omitted row counts and shortened text. Reaching a journal limit does not stop or change the file operation. The background worker attempts an atomic save before announcing operation completion. A save failure remains visible and preserves the previous file. This is a results log, not a crash-recovery journal: an operation interrupted by process termination or power loss may be missing.

History contains file paths, server names, usernames, and error messages. It does not record file contents or authentication secrets. Non-UTF-8 path characters use display replacements; history is not a source of exact paths for automated replay. Review the file before sharing it. Close PawCommander before manually removing history. A damaged or unsupported history file is preserved and disables saving until you fix or move it and restart. Only one running app instance can write history; another instance reports a save error. A history error does not change the file operation's outcome.

### Retry unfinished transfers

For a copy or move from the current session, expand its result and choose **Check and retry unfinished items**. The new attempt includes only failed, cancelled, skipped or unstarted selected objects for which the app retained source evidence. Completed objects, including those with cleanup warnings, and moves whose copy was published but source removal is incomplete are excluded. A selected folder is retried as a whole only if its root was not published; prepared files in its private temporary copy are not separate completed transfers.

Before doing any writes, the worker checks the original source metadata and destination location. It checks each source again after conflict decisions. A missing or replaced source or destination stops the retry with an explanation; inspect the files and start a new selection if the change was intentional. Existing destinations still use the normal conflict dialog. The copy-verification setting from the original attempt is retained, and retries use the same queue and Stop controls. A second click cannot enqueue the same attempt twice. A partly completed retry produces its own result and a new retry action for its remaining objects.

If an SFTP copy or move loses its publication reply, choose **Check interrupted SFTP transfer** in its result history entry. The check opens the connection, reads the original source and exact destination, and reports any retained staging, previous destination, or held source. A copy whose current contents match the source is reported without repeating the copy. For a move whose source has disappeared, the check can compare the destination tree with the metadata recorded before the rename. Matching metadata is evidence of the current state, and the original server reply remains unconfirmed.

If the check reports **Ready to retry**, use **Check and retry unfinished items** on the new check result. PawCommander verifies the source and destination again before writing and keeps the original destination name, including a name chosen with **Keep both**. A retained backup or a possible remaining source from a published move requires review. SFTP metadata describes the objects observed at each check.

Retry evidence stays in memory and is bounded by operation and item limits. After restarting the app, or for rename, create-folder and deletion results, review the saved history and select the intended files explicitly.

### Undo renames and moves

In **Result history**, expand a completed rename or move and choose **Undo available changes**. The count is the number of confirmed selected objects that can be checked for restoration. A folder counts as one object; its descendants are checked too. A partly completed move offers only its confirmed objects. Undo runs through the same queue, in reverse order within the selected operation, and supports **Stop**. Removing an unstarted Undo from the queue restores its action in history.

Undo uses the exact original paths and evidence kept in memory. Before each rename back, it checks the current object, descendants and both parent folders. If the old name is occupied, the object was replaced or changed, or a folder is inaccessible, it stops with an explanation and preserves existing files. It never offers Replace. Earlier successful restorations are not repeated; the original history entry retains an action only for the remaining eligible objects. A lost SFTP rename reply is marked unconfirmed and that object is excluded from another attempt: inspect both paths first.

**Undo is unavailable** for copy-and-delete moves, copies, deletion, folder creation, replacement of an existing destination, unverified results, and operations from before the current app session. It does not recreate the old version discarded by a successful Replace, restore a clipboard selection, or provide Redo. Evidence is bounded to 100 operations and 100,000 files and folders. Local checks use file identity and metadata; SFTP checks use canonical paths and server metadata, which do not provide a remote file identity, snapshot, or protection against every metadata-preserving external race. Undo supports local and same-server rename-based moves only.

## SFTP connections

1. Choose **SFTP** in PawCommander. Enter host, port, user, and starting folder. `.` means the server's home folder.
2. Supply an account password, a private-key file and optional passphrase, or leave password/key empty to use an SSH agent. An account password takes priority over the key.
3. When prompted to **Verify server key**, compare the displayed SHA-256 fingerprint with one obtained from the server administrator through a trusted channel. Select **Trust and connect** when they match. **Cancel** or `Esc` ends the attempt without recording that key.

Enter a hostname or `Host` alias in **Server** and select **Load SSH alias** to fill the SFTP form from `~/.ssh/config`. The importer reads `Host`, `HostName`, `User`, `Port`, and `IdentityFile`. Host patterns support `*`, `?`, and `!` exclusions. The first matching hostname, user, and port take precedence. When several identity files apply, use **Key files from SSH config** to choose one. You can review and edit the resolved fields before connecting. [OpenSSH client configuration](https://man.openbsd.org/ssh_config)

The form lists unsupported directives with line numbers, including `Include` and `Match`. When **Connect directly** appears, it uses the displayed fields. An existing key in `~/.ssh/known_hosts` can verify the endpoint. For ports other than 22, use the OpenSSH `[host]:port` form. [OpenSSH known hosts file](https://man.openbsd.org/sshd#SSH_KNOWN_HOSTS_FILE_FORMAT)

On macOS, load an alias with `ProxyJump` and choose **Connect via ProxyJump**. PawCommander resolves one directly reachable jump host from its own `Host` block. It uses that host's first `IdentityFile` if configured, or the SSH agent if no identity file is configured. Enter an encrypted key's phrase in **Jump host key passphrase**. Verify each server's fingerprint separately if prompted. The target server keeps its own credentials and the jump server must permit TCP forwarding to it.

Saved locations keep the resolved route. To reconnect automatically, the jump host needs an unencrypted key, an existing saved key for its own connection, or an SSH agent when no key file is configured. Use the original SSH alias for remote sessions in Terminal or Zed. [OpenSSH ProxyJump](https://man.openbsd.org/ssh_config.5#ProxyJump)

PawCommander remembers a key you approve for that host and port before sending authentication. On macOS these records are stored in `~/Library/Application Support/PawCommander/ssh-host-keys`. If the key changes, the connection stops and shows the received fingerprint and the relevant record path. For a planned rotation, verify the replacement fingerprint with the administrator, update the corresponding OpenSSH entry or remove the individual PawCommander record indicated by the error, then connect again and verify the fingerprint. When both sources contain an entry for the endpoint, both must agree.

When a verified SFTP server requests keyboard-interactive authentication, the connection dialog displays its instructions and questions. This also works when the server asks for a second factor after authenticating a key. Enter the answers in order and choose **Send answers**. The dialog hides input for questions that request hidden answers and shows how much time remains to respond. Each round allows up to 60 seconds within a three-minute interactive exchange. **Cancel** or `Esc` ends the attempt. The dialog uses answers for the current SSH exchange only. [SSH keyboard-interactive authentication](https://www.rfc-editor.org/rfc/rfc4256#section-3)

The address bar also accepts `sftp://user@example.com:22/path` with agent authentication or an existing connection. Use the SFTP form to approve a first-use key. Passwords in URLs are rejected. Remote absolute and relative paths belong to the server; use a local sidebar location to return to local files.

### Saved keys and privacy

Account passwords are not saved. If you opt to save a key and passphrase after connecting, a copy is stored in the OS credential store: Keychain on macOS, Credential Manager on Windows, or Secret Service on Linux. Saving happens after server verification, authentication, and opening the starting folder succeed.

UI settings contain connection profiles and key-file paths, but not passwords or private-key contents. These profiles, tabs, and bookmarks can still reveal hostnames, usernames, and local paths; do not share settings files without reviewing them. Removing a saved secret leaves the profile, original key file, and existing sessions intact. A locked or unavailable credential store causes an error rather than a plaintext fallback.

On Linux, a Secret Service provider must be running on the desktop session bus. On Windows, each saved key and passphrase must fit within Credential Manager's [2,560-byte limit](https://learn.microsoft.com/windows/win32/api/wincred/ns-wincred-credentiala), including UTF-8 JSON serialization. An oversized entry is rejected without replacing the existing saved entry; connect using the key file or SSH agent without saving it.

### Transfers and recovery

Regular files and folders can be copied recursively in either direction and between connections. Moves to or from local storage and between different connections use mandatory verification before guarded source removal. A same-connection move uses server rename. SFTP copying rejects symbolic links and special files.

SFTP copies and copy-based moves preserve ordinary permissions, the directory sticky bit, and the access and modification times supported by the destination. SFTP dates use whole seconds. Destination ownership follows the destination filesystem or account. On macOS, new move recovery records keep the original access times. If an older folder move record lacks those times or the destination cannot preserve an attribute, the transfer result reports partial preservation and the affected rows list the fields in **Details**.

The server must support `fstatvfs@openssh.com` free-space reporting and provide the metadata required for source checks. Unsupported reporting stops the current item before data transfer. Destination space checks may create and remove an empty probe in a private temporary directory.

A foreground request on a lost connection opens a fresh verified SSH session when the key or SSH agent can authenticate. Use the **SFTP** dialog to reconnect when a password, passphrase or MFA response is needed. A lost connection may leave a `.pawcommander-sftp-*` staging directory. Use the result history to inspect the recorded paths before retrying. SSH operations use a 20-second timeout and TCP connection attempts use 10 seconds per address. DNS resolution is handled by the OS.

If a connection fails during replacement or restoration, the server may have completed the rename before its reply was lost. Check both the original destination and the possible backup named in the error before retrying.

Remote deletion recursively removes directories without a trash. Symlinks are removed without following them. An interrupted or failed deletion may already have removed part of the selection. Visible remote folders refresh automatically through an existing connection; use `F5` for an immediate refresh.

### Change permissions

Select files or folders in an SFTP pane and choose **Change permissions** from the context menu or command palette. Enter an octal mode such as `0644` or a symbolic rule such as `u+rw,go-w`. A symbolic clause without a class applies to all classes (`a`). Choose **Preview** to see each item's current and proposed modes before choosing **Apply**.

The change applies to the selected regular files and folders. A selected folder's children keep their current modes. An unavailable item or a symbolic link is marked in the preview. If an item changes after the preview or the server denies access, the result history shows the reason for that item. Check any unconfirmed change before trying again after a disconnect.

## SMB connections on macOS

In the sidebar, choose **Connections → SMB · Connect** and enter a share address such as `smb://nas/Photos`. Enter the user name and password in their own fields, or select **Guest access**. If macOS requests access to network volumes, allow it to open the share in PawCommander.

Select **Save password in Keychain** to keep the password after a successful connection. Leave the password field empty when reconnecting to look up the saved password for that share and user. Connected shares appear under **Connections** and can be reopened after restarting PawCommander while macOS keeps them mounted. Use **Disconnect SMB** in the share's context menu to unmount it.

Use **Copy** to download files and folders from an SMB share. For uploads, select regular files and choose **Keep both** when an existing name matches. If a connection drops, wait for the operation to report its outcome and check the destination before retrying. An interrupted upload can leave an incomplete file with the chosen name. PawCommander preserves the source and existing destination files.

**Create archive** can save a new ZIP or TAR at an unused name on SMB. **Extract** can save individual files on SMB and offers **Keep both** for matching names. Use a local or SFTP destination for archive folders and a local folder for editing an existing archive.

## Custom actions on macOS

Choose **Settings → Tools → Custom actions…** to add a named action with an absolute path to its executable. Enter one argument per row. Use `{file}` in an argument for exactly one selected item, `{files}` on its own for one argument per selected item, or `{folder}` for the current folder. Use `{{` and `}}` to enter literal braces.

Select local files or folders and choose **Custom actions…** from the context menu or **Custom actions** from the command palette. Pick the action and review the executable, working folder and numbered arguments before choosing **Run**. PawCommander passes each value directly to the program, preserving spaces, quotes and other characters in paths.

To open selected files in an installed VLC player, use `/usr/bin/open` as the executable and enter `-b`, `org.videolan.vlc` and `{files}` in three separate rows.

## Previews and properties

Grid shows thumbnails for visible PNG, JPEG, and GIF files in local folders. Animated files show a still image. Thumbnails load in the background and update when a folder refresh detects a file change. Use `F5` to rebuild thumbnails in both panes, including after edits that preserve filesystem metadata. Images above 20 MiB, 8,192 pixels on either edge, or 16 megapixels keep their file icons. Icons also remain for links, unsupported or unreadable images, SFTP files, and archive contents.

Press `Space` on a file to open **Quick Look** in a separate window. Use `←` and `→` or **Previous File** and **Next File** to browse the visible files in their current sort order. Navigation skips folders and keeps the pane's selection. Press `Esc` or `Space`, or close the window, to return to the original file.

Choose **Hex** in Quick Look to inspect local or SFTP file bytes. Each row shows a hexadecimal offset, 16 bytes, and their printable ASCII characters. Browse 4 KiB pages or enter a hexadecimal **Offset (hex)** and choose **Go to offset**. The optional `0x` prefix is accepted. In **Find bytes**, enter up to 256 byte pairs with optional spaces, for example `DE AD BE EF`. **Enter** finds the next match and **Shift+Enter** searches backward. Search wraps, highlights the whole match across page boundaries, and can be canceled. The mode persists while browsing files in the same Quick Look window. Choose **Preview** to return to the regular view.

Quick Look and the details pane preview text and PNG, JPEG, GIF, and WebP images. The details pane reads up to 32 KiB from a text file and shows up to 1,000 lines and 8 KiB per line, with a notice when truncated. Images are limited to 20 MiB with additional dimension limits. Remote symlinks and special files are not read for preview.

On macOS, expand **Metadata** in Quick Look to see an image's original pixel dimensions and available EXIF data, including the camera, lens, date taken and exposure settings. The details pane also shows image EXIF. For audio and video, **Metadata** lists duration and available track details, including codecs, frame dimensions and rate, audio sample rate and channels. Missing values are marked **Unavailable**.

Use **Encoding** when a text file or table needs a different character encoding. **Auto** recognizes UTF-8 and uses the byte-order mark to select UTF-16 LE or BE. For files without a byte-order mark, Auto tries UTF-8. The menu also offers both UTF-16 byte orders, Windows-1250, Windows-1251, Windows-1252, KOI8-R, CP866, ISO-8859-2, and ISO-8859-5. The choice takes effect in the current preview and returns to Auto when you open another file. Changing it rereads the file and preserves the original bytes.

For text that exceeds the prefix, Quick Look opens a paged view. Use **Previous page**, **Next page**, or enter a **Line number** and select **Go to line**. Line numbers follow the original file, including when a long line continues across pages. In **Find in file**, press **Enter** to jump to the next literal, case-sensitive match or **Shift+Enter** to jump backward. Search covers the entire file, wraps at the ends, and can be canceled while running. Quick Look reads bounded sections of local and SFTP files in the background. For large Markdown and HTML files, use **Source text** for paging and whole-file search.

On macOS, the details pane shows the first page of local and SFTP PDF files. Open **Quick Look** to turn pages with the page controls or `Page Up` and `Page Down`, jump to a page by number, and choose **Fit page** or a zoom percentage. Use **Find in PDF** or `Cmd+F` to search the document’s text. Search ignores case and highlights matches on the displayed page. The match buttons move between pages with hits. Scanned pages render as images and can be searched when the PDF includes a text layer. The preview supports PDFs up to 64 MiB and 10,000 pages. A locked, unreadable, or stalled document shows an error in the preview.

On macOS, open **Quick Look** on an audio or video file and choose **Play**. Use **Pause** and the position slider to control playback. The timer shows the current position and total duration when available. Switching files or closing the window stops playback and releases the player. Quick Look supports MP4, M4V and MOV video and AAC, AIFF, CAF, FLAC, M4A, M4B, MP3 and WAV audio using the codecs available on macOS. SFTP media up to 128 MiB is downloaded to a temporary file when Quick Look opens and removed when the preview ends.

Open a local or SFTP CSV or TSV in **Quick Look** to see its columns. **Separator** detects commas, semicolons and tabs. Select a separator manually if needed. Quoted cells can contain separators, doubled quotes and line breaks. Turn off **First row has headers** when every row is data. Scroll horizontally for wide tables and drag column dividers to resize them. **Previous rows** and **Next rows** read pages of up to 128 rows. **Filter rows** matches literal text in any cell, ignoring case, and searches beyond the current page. Rows are read in the background. Each record can contain up to 256 columns and 256 KiB of decoded text. The details pane shows a text prefix.

Text previews include line numbers and basic lexical highlighting for Rust, JavaScript/TypeScript, Python and JSON. Other text files use plain text. Highlighting recognizes strings, comments, keywords and numbers, rather than semantic types or full language grammars (for example JavaScript regular expressions and template interpolation are not parsed). Reading and tokenization run in the background. Where shown, **Find in preview** searches the displayed text for a case-sensitive literal match. **↑** / **↓** or `Shift+Enter` / `Enter` move between matches and scroll to them. Matches are highlighted; select text to copy it without line numbers. Long lines scroll horizontally. Search and selection reset when the preview reloads or changes file. While a preview text field has keyboard focus, Quick Look leaves Space, Escape and arrow keys to that field; use the window buttons to close or browse files.

Markdown (`.md`, `.markdown`, `.mdown`, `.mkd`) and HTML (`.html`, `.htm`) open in **Formatted** view in both Quick Look and the details pane; extensions are case-insensitive. The native text renderer supports headings, paragraphs, emphasis, inline/fenced code, lists, quotes, rules, and simple table rows separated by `|`. Markdown uses CommonMark with tables and strikethrough; raw HTML in Markdown is shown literally. HTML uses a limited text-formatting subset rather than browser layout: CSS, scripts, forms, embedded content and document metadata are ignored. Links are inert text and images are alt-text placeholders. No linked images, stylesheets, fonts, media or other resources are loaded, including local and remote URLs. Previewing does not modify the source file.

Choose **Source** for the existing line-numbered text view and Find controls; switching views resets search, selection and scroll. Parsing runs in the background on the same bounded text prefix. Formatting also stops at 32 nested elements, 8,192 parser events, 4,096 text runs, 1,024 blocks or 64 KiB of output, with an explicit complexity notice. Malformed or complex documents may have simplified or incomplete formatting; Source still shows the bounded input prefix. This is not a full browser, CSS renderer or implementation of every Markdown extension.

Select items and use **Properties** or `Alt+Enter` to calculate their combined logical size and counts in the background. This works for local items and, on macOS, items on the same SFTP connection. The calculation includes hidden entries and counts overlapping selected trees once. Each hard link counts per path, and symbolic links are counted separately without traversing their targets.

Properties show available dates and permissions for selected items. Unix permissions show the mode, UID, and GID. Windows shows read-only state. SFTP shows the server's modification and access times, with creation time marked unavailable. The SFTP calculation can read up to 128 nested directory levels.

On macOS, local Properties also show each selected item's file system, the hard-link count for regular files, Finder tag names, extended attribute names and ACL entries. Properties read attributes on a symbolic link itself. An empty field, a volume that lacks support and a read error have distinct messages. Large attribute lists and tag data can exceed the preview limit. See [transfer behavior](#copying-moving-and-conflicts) for which metadata copies preserve.

While counting, **Stop calculation** receives initial keyboard focus. Press `Space` or `Enter` to stop and keep the partial result. Close the window or press `Esc` to dismiss it and request cancellation. Cancellation takes effect between filesystem or SFTP requests. When an entry disappears or cannot be read, the window identifies the result as partial.

### Create and inspect links on macOS

Choose **Create link** from the command palette or a local file's context menu. Enter a new name in the active folder, select **Symbolic link** or **Hard link**, and set the target path. A single selection fills in the target. Relative paths start in the folder where the link will appear. The name must be unused. Hard links require a regular file on the same volume. A symbolic link may point to a folder or a path that does not exist yet.

An error mark and tooltip identify a symbolic link with a missing, unavailable or looping target. **Properties** shows the stored target path and its current status. Recursive listings and counts skip symbolic-link branches. You can open a working folder link directly.

## Compare folders

Open the folders in the two panes and choose **Compare folders** from the command palette or the Settings menu. The comparison looks at exact relative names throughout both folder trees, including hidden files. Results show items present on one side, different files and type conflicts. **Show matches** includes the matching files and links. Unreadable folders appear as one error row for the affected branch.

**Size and modified time** compares file sizes and modification times. Comparisons involving SFTP use whole seconds. **By contents** compares the SHA-256 hashes of equal-length files. The comparison treats each symbolic link as a single entry and compares its target path. Comparing is read-only. Use **Stop** to end the scan. The scanner supports up to 100,000 entries across both folders, 32 MiB of path data and 128 directory levels.

On macOS, a comparison stops if a saved SMB volume disconnects or changes. The error names the affected pane and share. An individual branch or file on another volume appears as a read error while the rest of the comparison continues.

## Find duplicates on macOS

Open a local folder and choose **Find duplicates** from Settings or the command palette. Start the scan to search that folder and its subfolders. Equal file sizes select candidates for SHA-256 hashing. Equal hashes are then checked byte for byte. The search compares the data fork of regular files. It skips symbolic links and PawCommander staging folders. Hard links to one inode count as one file. The scan supports 100,000 entries, 32 MiB of path data and 128 directory levels and reports changed or unreadable entries.

Each group starts with one file marked **Keep**. Select a different one if desired. Manually mark the other files you want to move to Trash, choose **Review selected**, then confirm. PawCommander checks the versions and contents again before moving a marked file to the system Trash. If anything changed, inspect the reported path and run a fresh search. To restore a file from Trash, drag it back to its original folder shown in Result history.

## Analyze disk usage on macOS

Open a local folder and choose **Analyze disk usage** from Settings or the command palette. **Size tree** shows each folder's total and lets you expand its children. **Largest files** ranks regular files by allocated space and shows exact allocated bytes and file length in the tooltip. **Disk map** uses area to show relative sizes. Select a folder tile to inspect that branch, or use **Parent folder** to return. The map groups smaller entries under **Other entries**.

The scan counts allocated blocks reported by macOS for files and folders, skips symbolic-link targets and counts hard links once, under the first path in name order. APFS clones can share blocks, so the values are estimates of space attributed to the scanned entries. **Stop** keeps the sizes already counted. Incomplete folder sizes are marked as partial, and folders the scan could not measure show **No data**. Expand **Items with scan errors** for the full paths and causes of up to four affected items. When the scan stops at a limit, it displays the limit and the path where it stopped. The scan supports 100,000 entries, 32 MiB of paths and 128 folder levels.

## Tags and collections on macOS

Select local files or folders and open **Tags and collections** from Settings or the command palette. Create or choose a collection, then click **Add selection**. Enter comma-separated PawCommander tags when adding several objects. Select an existing row to edit its tags, use **Filter by tag** to find tagged items, or choose **Show in pane** to select the item at its current location. Removing an item or a collection removes its saved references. The files and folders stay where they are.

Opening a collection or clicking **Refresh** checks its references. PawCommander finds renamed or moved objects on volumes with stable file identifiers, including APFS. A missing item stays in the collection with its last known path and an explanation. To reconnect it, select the intended file or folder in the pane, open the collection, select the unavailable row, and choose **Relink to selection**.

To transfer a collection, open its common parent folder first. Under **Import and export collections**, enter a filename ending in `.pawcollection.json` and choose **Export to this folder**. The export includes paths relative to that folder and the assigned tags. Copy the exported file together with the referenced directory structure. In the destination folder, select the JSON file, open **Tags and collections** and choose **Import selected file**. All paths must resolve within that folder and have the expected file or folder type. An existing collection with the same name keeps its entries, and the imported collection receives a numbered name.

An import or export error names the collection file. If a particular entry caused the error, the message also shows its position and path. Export preserves files already using the chosen name.

Portable collections also work on SMB shares. If a PawCommander connection drops or the mounted share changes during import, the collection stays out of the catalog. An SMB export uses an unused filename. If the connection drops after creation and the export reports an unconfirmed write, check the new file before retrying.

## Create files from templates on macOS

Open a local or SFTP folder and choose **New file** from Settings or the command palette. Choose blank text, Markdown, JSON, CSV or a saved template and enter the file name. Built-in templates let you edit their contents and select UTF-8, UTF-8 with BOM, UTF-16 LE or BE with BOM, Windows-1251 or Windows-1252. If the text contains characters that the chosen encoding cannot represent, the dialog asks you to change the encoding.

To reuse a file, select it in the pane before opening **New file** and choose **Selected file** as the template. **Save as template** stores a copy in PawCommander's settings for later use. You can also save an edited built-in template. File-based templates preserve the original bytes and accept files up to 4 MiB. After a name conflict, the form keeps your draft so you can enter a different name. PawCommander creates the file without replacing any existing item and selects the new file in the pane.

The library supports up to 64 templates and 16 MiB of saved template data. If saving fails, the dialog shows the source error or the limit reached. The draft remains available for correction.

## Transfer with compatible names on macOS

Open two local folders, select regular files or folders in the source pane, and choose **Transfer with compatible names** from Settings or the command palette. Select **Copy** or **Move**, then choose **Preview names**. The preview lists each proposed path and the reason for the change, including changes within selected folders. Choose **Copy with these names** or **Move with these names** to apply the reviewed plan.

The preview replaces Windows and FAT forbidden characters, prefixes reserved device names, trims trailing dots and spaces, and composes Unicode as NFC. It shortens names for the destination's length limits and uses numbered suffixes for case or Unicode collisions. Compatible names already in the selection keep priority over rewritten variants. The operation checks the source tree and the destination names again before applying the plan. [Windows file naming](https://learn.microsoft.com/en-gb/windows/win32/fileio/naming-a-file), [exFAT filename rules](https://learn.microsoft.com/en-us/windows/win32/fileio/exfat-specification), [Unicode normalization](https://www.unicode.org/reports/tr15/).

On FAT and exFAT, the preview reserves space and names for macOS `._` metadata files. It gives selected files using that prefix a distinct name on the destination. On exFAT, files appear as PawCommander copies them. After an interruption, the result identifies any unfinished output for inspection. [AppleDouble file format](https://datatracker.ietf.org/doc/html/rfc1740#section-5).

**Move** uses an exclusive rename to hold the original tree and verifies the copy before removing files. For a FAT, exFAT or read-only source, choose **Copy**. This command transfers the contents of local regular files and folders and attempts to preserve permissions and timestamps. The plan is limited to 100,000 entries and 32 MiB of paths. The destination limit counts macOS companion files when applicable.

## One-way folder synchronization on macOS

Open the two folders and choose **Folder sync preview** from Settings or the command palette. Select the source direction and choose **Build plan**. The preview lists each planned copy, replacement and deletion by relative path. It compares regular files by SHA-256 and symbolic links by their target paths.

Enable **Remove destination-only items** to include extra files and folders from the destination. When a file replaces a folder, the preview lists the affected descendants as deletions. **Check plan** reads both folders again and identifies the changed path. Build the plan again after editing either folder. If a read fails or an entry changes during planning, the preview shows which folder was being read, the path and the cause. Overlapping folders, name collisions and unreadable entries prevent a complete plan.

Choose **Apply plan** to queue the displayed changes. When the job starts, PawCommander checks both folders again. If the plan has become stale, the job ends before changing files. Copies and replacements are checked in private staging before publication. The job processes destination-only deletions after the copies and replacements. Local deletions go to the Trash, and deletions from an SFTP folder are permanent.

Use **Stop** to stop before the next change publishes. **Result history** shows completed paths and the active operation's outcome, path and cause. A failed check of a staged copy or held original identifies the item and the temporary path where it was checked. If publication or restoration cannot be confirmed, the result gives the temporary folder to inspect.

## Two-way folder synchronization on macOS

In **Folder sync preview**, choose **Both directions** and **Build plan**. The first comparison shows copies for items present on only one side and flags different versions of the same path. If the folders already match, choose **Save common state**. PawCommander checks both folders and saves fingerprints for that pair. The same record is used when the panes are swapped or the app is reopened.

Later plans use the saved state to distinguish new items from deletions. If both versions changed, choose which version to keep. When an item was edited on one side and deleted on the other, choose the edited version to restore the item or accept the deletion. A choice for a disputed folder applies to its contents. Review the resulting actions before choosing **Apply plan**. The queue rechecks both folders and saves the new common state once both match the planned result. Local deletions go to the Trash. SFTP deletions are permanent.

After a stop or interruption, build a fresh plan for the same two folders. Differences from an interrupted run require a choice before you can apply the new plan. If PawCommander reports a retained staging folder, use the displayed path to inspect the copies. An original held during replacement or deletion is under `previous` inside that folder. Restore it if its original name is free. Save any other needed versions outside both synchronized folders and move the retained staging folder out of the pair before building a new plan. If the folders match after recovery, **Save common state** completes the review.

If PawCommander cannot read or save the common state or run record, the message identifies the affected file and cause. **Result history** keeps file changes that already completed. Once the storage issue is resolved, build a fresh plan to check the current state of both folders.

## Sync profiles on macOS

Open the two local or SFTP folders, then open **Folder sync preview**. Choose the direction and exclusions, then use **Save profile** to name this setup. A profile keeps the two folder locations, the direction, exclusions and the one-way option to remove destination-only items. You can build a fresh preview for a saved pair while browsing elsewhere. Use **Use folders from panes** to replace a saved pair and **Update profile** to save edits or rename it.

Enter one exclusion per line. A name such as `*.tmp` or `.git` matches at any depth. Use `cache/previews` for a path from the folder root, or `./cache` to match `cache` only at the root. `*` and `?` match within a name. Matching is case-insensitive. Excluding a folder also excludes its contents. The preview counts each excluded folder once per pane. Deleting or replacing a folder that contains an excluded item stops planning and identifies the protected path.

The list supports up to 64 distinct rules within 8 KiB of text. The preview and profile editor show the line to fix when an exclusion is invalid or adds a rule beyond the count limit.

Changing the exclusion rules starts a new comparison scope for two-way sync. The next verified save or successful application establishes the shared state for that scope.

## Compare files

Select one regular file in each pane and choose **Compare files** from the command palette or Settings. Local and SFTP files can be compared. The result shows both sizes and the first differing byte. For text, the dialog shows additions, removals and neighboring lines with their original line numbers. **Show matches** expands the rest of the text. Byte differences remain visible when decoded lines match across encodings or line endings.

The built-in comparison also works with files on SMB shares in macOS. If a saved volume disconnects or changes, the window reports the affected pane, file and share.

Text comparison accepts UTF-8 and auto-detects UTF-16 with a byte-order mark. It supports up to 2 MiB and 30,000 lines per file. The viewer shortens very long displayed lines and can group extensive edits into one block. Comparison by bytes also works for binary files and files beyond the text limits. Use **Stop** to end the comparison. If a file becomes unavailable or changes, the dialog shows the affected pane, path and cause. Choose **Compare again** after resolving the problem.

For two local files, **Open in external tool** hands both paths to the executable set under **Settings → Tools**. The executable accepts the left and right paths as its two arguments. An empty setting uses FileMerge when PawCommander finds it. Use **Check again** to refresh automatic detection. [Apple's FileMerge command reference](https://developer.apple.com/documentation/xcode/xcode-command-line-tool-reference)

## SHA-256 manifests

Select regular files in a local or SFTP folder and choose **Create SHA-256 manifest**. Enter a new filename to save their checksums in the same folder. The default is `SHA256SUMS`. The operation preserves existing files and publishes the manifest after reading all selected inputs. The output uses the [GNU checksum format](https://www.gnu.org/software/coreutils/manual/html_node/cksum-output-modes.html).

Select a manifest and choose **Verify SHA-256 manifest** from the command palette. The context menu also offers this action for `SHA256SUMS` and `.sha256` files. Verification reads paths relative to the manifest's folder and accepts standard untagged and tagged SHA-256 records. Paths through symlinks receive a read error. Invalid paths and duplicate entries cause a manifest error. Up to 16 MiB and 50,000 entries are supported. Use **Stop** to cancel and **Result history** to see matches, changed files, missing files and read errors.

On macOS, manifests can also be created and verified on SMB shares. Creation uses an unused name. If the saved SMB volume changes during verification, the history keeps the completed checks and shows where verification stopped.

## Search and Zed

`Cmd/Ctrl+F` filters names in the current folder. `Cmd/Ctrl+Shift+F` opens search. **Folder and subfolders** starts in the local or SFTP folder that was open when you opened Search. Enter a name fragment or a wildcard such as `*.rs`. Comma-separated exclusion masks skip matching names and prune matching directories. Folder search skips symlink branches and returns up to 1,000 results. It reports incomplete coverage if it cannot read a branch or reaches the traversal limit. You can cancel the search and reveal a result in its parent folder.

SFTP search matches names and uses the existing authenticated SFTP connection. Each result starts with the file or folder name. Hover over a result to see its server-resolved SFTP address. The default depth is 8, with direct children at depth 1. Change it under **Filters**. Set **SFTP search time limit** to 1–60 seconds, with 15 seconds as the default. A search examines up to 20,000 directory entries. At the time or traversal limit, PawCommander shows the results collected so far. Cancel and the time limit stop the walk between SFTP requests. An in-flight request can still take its normal SFTP timeout.

**Locate index** uses `locate` or `plocate`, with a 15-second wait limit for the query. The search window and results pane show the database paths PawCommander can determine and their file modification times as freshness estimates. The index can omit restricted folders and contain stale paths. When you try to reveal a missing result, PawCommander removes it from the current list and keeps the search open. Check **Settings → Tools** to see detected tools. On Windows, compatible command-line tools must be available in `PATH`.

In a local folder, choose **File text** for a literal query or **Regular expression** for a regex. Matching runs line by line and is case-sensitive by default. Results show the path, line number, and a text excerpt. Search skips files over 1 MiB, files containing a NUL byte, and files with invalid UTF-8. Regex search also skips lines over 64 KiB. The result list holds up to 1,000 matching lines.

Open **Filters** to combine entry type, extension, size, modification date, and depth with your query and exclusions. Size and extension apply to regular files. Size bounds are inclusive and share the selected unit. Enter dates as `YYYY-MM-DD` in local time. The **Modified through** date includes the whole day. Set a maximum depth from `1` to `64`, where `1` searches the starting folder's immediate children. You can leave the **File name** query empty when a filter is set.

Choose **Save search…** and enter a name to reuse the query, scope, exclusions, and filters after restarting PawCommander. Select the name under **Saved searches** to run it again. Folder searches scan the saved folder even when a pane is open elsewhere. A saved SFTP search also restores its time limit and requires a connection to the saved server. A saved Locate search queries the currently configured index. Use the **…** menu to rename or remove a saved search.

Choose **Show in results pane** to keep the current results visible while you browse other folders. Select a result to open it or show it in the active pane. Select one or more files and choose **Copy to the other pane** to copy to the displayed destination. Multiple matching lines from a file select it once. If a selected path has disappeared, its rows are removed. Review the remaining selection before retrying a copy.

**Open in Zed** sends a local path or an SSH URL to Zed. Normal opening of a remote file uses a bounded content check to recognize text (UTF-8 or BOM-marked UTF-16); explicitly choosing Zed bypasses that check. Other remote files must be copied locally before opening in external apps.

Zed establishes its own SSH connection using its SSH configuration and authentication. PawCommander's stored secrets are not passed to Zed. A server must allow full SSH access and execution of Zed's helper; SFTP-only access is insufficient. Zed may install its remote server under `~/.zed_server`. Edits are saved on the server; the visible folder updates automatically, or use `F5` to refresh immediately. See [Zed Remote Development](https://zed.dev/docs/remote-development).

## Open a terminal in the current folder

On macOS, right-click the current folder in the pane's address bar and choose **Open Terminal Here**. The same command is available in the command palette (`Cmd+Shift+P`). Apple Terminal opens in the current local folder.

From an SFTP folder, the command opens a separate interactive OpenSSH session in Terminal. The server must allow an SSH shell and a PTY. OpenSSH uses the host, port, account and optional key-file path from the SFTP location. It handles host trust and authentication using its own configuration, agent and prompts. You can enter the password or unlock the key in Terminal if prompted. Key-file names containing OpenSSH expansion syntax use its `IdentityFile` environment expansion, available in OpenSSH 8.4 and later.

### Embedded terminal

On macOS, choose **Open Embedded Terminal** from the same folder menu or from the command palette. The bottom panel starts an interactive local shell or SSH session in the selected pane's current folder. The header shows that starting folder. Opening the action again focuses the running session. Run `cd` inside the shell to change its working folder while browsing in the file panes.

Drag the panel's top edge to resize the terminal and its PTY. Use the trackpad or wheel to scroll through output, drag to select text, and use `Cmd+C` or `Cmd+V` to copy or paste. `Ctrl+C` goes to the terminal session. **Close Terminal** ends the managed session and closes the panel. When the shell exits, its output stays available until you close the panel or open a new session.

## Accessibility

Open **Settings → Accessibility** to choose an interface scale from 90% to 150% or enable high contrast. Both choices are saved. `Cmd/Ctrl+=` and `Cmd/Ctrl+-` change the scale, and `Cmd/Ctrl+0` resets it to 100%.

Press `F10` to focus Settings on the toolbar. `Tab` and `Shift+Tab` move between controls, and `Enter` or `Space` activates the focused button. In the file area, Up/Down selects and focuses files, and `Shift` with Up/Down extends the selection. `Shift+Space` toggles selection of the focused row, and `Enter` opens it. The default `Tab` binding switches panes from the file area when both panes are visible.

List rows and grid tiles expose their full names, types, and selection states to screen readers. Screen reader activation toggles a file's selection. Keyboard focus can move to a file without changing the selection.

## Keyboard shortcuts

On macOS, open **Settings → Help…** for a quick orientation and a searchable command reference. Search by words in the command name to see its current binding, including changes you saved in Keyboard Shortcuts. Use **Open Command Palette** to keep your search and run an available command.

Open **Settings → Command Palette** or press `Cmd/Ctrl+Shift+P`. Type words from a command name to filter the list, use `↑` or `↓` to select an available command, and press `Enter` to run it. Press `Esc` to return to the previous control. The list shows the shortcuts from your active profile, including custom bindings. You can run a command from the palette even if it has no shortcut.

Open **Settings → Keyboard Shortcuts** to view and change bindings. Click a binding and press the new shortcut. If it is already assigned, PawCommander shows which action uses it. Clear that action's binding first if you want to reassign the key. Press `Esc` to stop recording. Use **Save** to apply and remember your changes, or **Cancel** to discard them.

The **Commander** profile changes the function row. `F3` toggles the details panel, `F4` opens the selected file in Zed when available, `F5` copies to the other pane, `F6` moves to the other pane, `F7` creates a folder, and `F8` opens the deletion confirmation. Refresh moves to `Cmd/Ctrl+R` and the dual-pane toggle moves to `Cmd/Ctrl+Shift+D`. Copying and moving between panes require both panes to be visible.

Choosing another profile replaces custom bindings in the editor. **Reset to defaults** restores the Default profile. Save to keep either change. Clipboard shortcuts, Up/Down selection and `Esc` are fixed. Use a function key, a supported navigation key, or a key with a modifier. System and native menu shortcuts may take precedence. On macOS this includes `Cmd+H`, which can hide the app.

### Default profile

| Action | Shortcut |
| --- | --- |
| Show / hide the second pane | `F3` |
| Switch panes | `Tab` (from the file area) |
| Focus toolbar | `F10` |
| New / close tab | `Cmd/Ctrl+T` / `Cmd/Ctrl+W` |
| Back / forward | `Alt+Left` / `Alt+Right` |
| Parent folder | `Alt+Up` |
| Enter a path | `Cmd/Ctrl+L` |
| Filter current folder | `Cmd/Ctrl+F` |
| Search local or SFTP folders, or indexed paths | `Cmd/Ctrl+Shift+F` |
| Open the command palette | `Cmd/Ctrl+Shift+P` |
| Show / hide hidden files | `Cmd/Ctrl+H` |
| Refresh | `F5` |
| Select all | `Cmd/Ctrl+A` |
| Copy / cut / paste | `Cmd/Ctrl+C` / `Cmd/Ctrl+X` / `Cmd/Ctrl+V` |
| Move on paste (macOS) | `Option+Cmd+V` |
| Rename | `F2` |
| Local properties | `Alt+Enter` |
| Create folder | `Cmd/Ctrl+Shift+N` |
| Delete with confirmation | `Delete` / `Cmd/Ctrl+Backspace` |
| Open focused or selected item | `Enter` |
| Quick Look | `Space` |
