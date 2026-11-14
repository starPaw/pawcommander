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

Choose a language under **Settings → Language**. English is the default; Russian is also available. File names and contents are unchanged. Choose Latte, Frappé, Macchiato, or Mocha under **Settings → Catppuccin**. System appearance switches between Latte and Frappé.

Opening a local or SFTP folder shows entries progressively while **Reading folder…** remains visible. File metadata is read in the background. Navigating away, refreshing, or closing the tab cancels the previous read between filesystem or SSH calls; an already blocked call must finish or time out first. If a read fails after showing entries, the partial listing stays visible with an error. Refreshing a previously complete listing replaces it as a whole. Large folders reuse the current filtered and sorted view between updates. The loaded listing stays in memory, so usage grows with the number of entries and the length of their names.

Visible folders refresh in the background after the previous read finishes: approximately every two seconds locally and every ten seconds for SFTP. Selection is retained by path; filters, sorting, and scroll offsets are preserved. Hidden panes and inactive tabs are not polled. SFTP polling uses an existing authenticated session and skips a busy refresh or unavailable session without clearing the listing or prompting for credentials. Automatic refresh pauses during file operations and dragging. Large or slow folders may take longer. Use `F5` to refresh manually. The selected local file's preview reloads when observed metadata changes; this is not a content watcher.

## Browse ZIP and TAR archives

Double-click a local `.zip` or uncompressed `.tar` file, or select it and press `Enter`. PawCommander opens the archive in the current pane and reads its directory structure and metadata. **Open** in the search results pane also opens the archive in the active pane. Browse nested folders from the file list. From the archive root, use **Parent folder** to return to the folder containing the archive. Use **Back** and **Forward** to revisit locations, and **Refresh** to reread an archive after it changes.

Archive browsing is read-only. Entries stay inside the archive while you browse. PawCommander reports unreadable archives and invalid member paths in the pane. The browser indexes up to 50,000 items, including implied folders, and supports up to 64 path levels.

## Selection, clipboard, and drag and drop

Click a row or tile to select it, use `Cmd/Ctrl` for individual additions and `Shift` for a range, or use the selection checkboxes. File commands apply to the active pane.

Use the command palette to select by mask or extension, invert the selection, or restore the previous selection. A mask matches the whole name and supports `*` for any number of characters and `?` for one character. Extension matching uses the final extension. Matching ignores case. These commands work on visible entries in the active tab. You can restore the previous selection once after applying a rule, inverting, selecting all, or clearing with Esc. Restore uses available metadata to keep matching entries in the current folder.

Copy and cut local files to the system clipboard with `Cmd/Ctrl+C` / `X`; paste with `Cmd/Ctrl+V`. If the OS clipboard is unavailable, PawCommander reports it and falls back to its internal clipboard. SFTP sources use the internal clipboard.

- **macOS:** copy with `Cmd+C` and move at the destination with `Option+Cmd+V`, including between Finder and PawCommander. A cut inside PawCommander can also be moved by ordinary paste inside PawCommander.
- **Windows:** Explorer and PawCommander exchange local file paths and copy/move intent. Virtual items without local paths are not supported.
- **Linux:** Nautilus and Dolphin clipboard formats are supported. X11/XWayland needs an accessible clipboard; Wayland requires `ext-data-control` or `wlr-data-control`. Keep PawCommander open until the receiving app has pasted.

On macOS, file paste starts when `V` is pressed; you can release `Cmd` and `Option` in any order. Text fields keep the usual `Cmd+C`, `Cmd+X`, and `Cmd+V` behavior.

Drag a selection onto a folder, pane, tab, or breadcrumb. Internal drops copy by default; holding `Shift` moves local items or items within the same SFTP connection. Cross-connection and local/remote drops copy only. `Esc` cancels an internal drag.

Incoming drops from a system file manager always copy. To drag local files into another application, drag the selection across the window edge. Outgoing native drags offer copy on macOS, Windows, Linux X11 and Wayland; the receiving app may copy, open or attach the files. Shift does not authorize an external move. Press Esc to cancel. Returning an outgoing Windows/Linux drag to PawCommander cancels that handoff; start a new internal drag to transfer between panes. The receiving app handles its own conflicts and copy progress. Remote items must first be copied locally; remote file promises are not implemented.

Wayland outgoing drag uses the window's active pointer press and requires `wl_data_device_manager` version 3. It does not require the clipboard's data-control extension. Wayland clipboard copy/cut/paste separately needs `ext-data-control` or `wlr-data-control` as described above.

## Copying, moving, and conflicts

The operation panel shows the current item, elapsed time, and, when available, copied bytes, speed, and an estimate for the current file. One file operation runs at a time; browsing remains available. Further copies, moves, folder creation, renames, and deletions join a FIFO queue with room for 100 pending jobs. Expand **Queued** in the footer to inspect the order, move a pending job earlier or later, or **Remove from queue** before it starts. Removing a pending job makes no filesystem changes and creates no result-history record.

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

An unconfirmed publication, such as a lost SFTP rename reply, is never retried automatically. Check both locations and the recovery report. SFTP source checks use the metadata the server provides; they do not establish a filesystem snapshot. Retry evidence stays in memory, bounded to 100 operations and 100,000 selected roots. After restarting the app, or for rename, create-folder and deletion results, review the saved history and select the intended files explicitly; saved display paths are not replayed.

### Undo renames and moves

In **Result history**, expand a completed rename or move and choose **Undo available changes**. The count is the number of confirmed selected objects that can be checked for restoration. A folder counts as one object; its descendants are checked too. A partly completed move offers only its confirmed objects. Undo runs through the same queue, in reverse order within the selected operation, and supports **Stop**. Removing an unstarted Undo from the queue restores its action in history.

Undo uses the exact original paths and evidence kept in memory. Before each rename back, it checks the current object, descendants and both parent folders. If the old name is occupied, the object was replaced or changed, or a folder is inaccessible, it stops with an explanation and preserves existing files. It never offers Replace. Earlier successful restorations are not repeated; the original history entry retains an action only for the remaining eligible objects. A lost SFTP rename reply is marked unconfirmed and that object is excluded from another attempt: inspect both paths first.

**Undo is unavailable** for copy-and-delete moves, copies, deletion, folder creation, replacement of an existing destination, unverified results, and operations from before the current app session. It does not recreate the old version discarded by a successful Replace, restore a clipboard selection, or provide Redo. Evidence is bounded to 100 operations and 100,000 files and folders. Local checks use file identity and metadata; SFTP checks use canonical paths and server metadata, which do not provide a remote file identity, snapshot, or protection against every metadata-preserving external race. Undo supports local and same-server rename-based moves only.

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

On Linux, a Secret Service provider must be running on the desktop session bus. On Windows, each saved key and passphrase must fit within Credential Manager's [2,560-byte limit](https://learn.microsoft.com/windows/win32/api/wincred/ns-wincred-credentiala), including UTF-8 JSON serialization. An oversized entry is rejected without replacing the existing saved entry; connect using the key file or SSH agent without saving it.

### Transfers and recovery

Regular files and folders can be copied recursively in either direction and between connections. Rename is supported within one connection. Moves to or from local storage and between different connections use mandatory verification before guarded source removal, as described above. A same-connection move still uses server rename. SFTP copying rejects symbolic links and special files and does not preserve source ownership, ACLs, permissions, or dates.

The server must support `fstatvfs@openssh.com` free-space reporting and provide the metadata required for source checks. Unsupported reporting stops the current item before data transfer. Destination space checks may create and remove an empty probe in a private temporary directory.

A lost connection may leave a `.pawcommander-sftp-*` staging directory; the error identifies its path. Inspect the destination and staging directory before retrying. Reconnect with the SFTP dialog. There is no transfer resume. SSH operations use a 20-second timeout; TCP connection attempts use 10 seconds per address. DNS resolution is handled by the OS.

If a connection fails during replacement or restoration, the server may have completed the rename before its reply was lost. Check both the original destination and the possible backup named in the error before retrying.

Remote deletion recursively removes directories without a trash. Symlinks are removed without following them. An interrupted or failed deletion may already have removed part of the selection. Visible remote folders refresh automatically through an existing connection; use `F5` for an immediate refresh.

## Previews and properties

Grid shows thumbnails for visible PNG, JPEG, and GIF files in local folders. Animated files show a still image. Thumbnails load in the background and update when a folder refresh detects a file change. Use `F5` to rebuild thumbnails in both panes, including after edits that preserve filesystem metadata. Images above 20 MiB, 8,192 pixels on either edge, or 16 megapixels keep their file icons. Icons also remain for links, unsupported or unreadable images, SFTP files, and archive contents.

Press `Space` on a file to open **Quick Look** in a separate window. Use `←` and `→` or **Previous File** and **Next File** to browse the visible files in their current sort order. Navigation skips folders and keeps the pane's selection. Press `Esc` or `Space`, or close the window, to return to the original file.

Quick Look and the details pane preview UTF-8 text and PNG, JPEG, GIF, and WebP images. Text previews show a bounded prefix (up to 32 KiB, 1,000 lines and 8 KiB per line), with an explicit notice when truncated, and images are limited to 20 MiB with additional dimension limits. Remote symlinks and special files are not read for preview.

Text previews include line numbers and basic lexical highlighting for Rust, JavaScript/TypeScript, Python and JSON; other UTF-8 files use plain text. Highlighting recognizes strings, comments, keywords and numbers, rather than semantic types or full language grammars (for example JavaScript regular expressions and template interpolation are not parsed). Reading and tokenization run in the background. Use **Find in preview** for case-sensitive literal search of the displayed prefix; **↑** / **↓** or `Shift+Enter` / `Enter` in the search field move between matches and scroll to them. Matches are highlighted; select text to copy it without line numbers. Long lines scroll horizontally. Search and selection reset when the preview reloads or changes file. While a preview text field has keyboard focus, Quick Look leaves Space, Escape and arrow keys to that field; use the window buttons to close or browse files.

Markdown (`.md`, `.markdown`, `.mdown`, `.mkd`) and HTML (`.html`, `.htm`) open in **Formatted** view in both Quick Look and the details pane; extensions are case-insensitive. The native text renderer supports headings, paragraphs, emphasis, inline/fenced code, lists, quotes, rules, and simple table rows separated by `|`. Markdown uses CommonMark with tables and strikethrough; raw HTML in Markdown is shown literally. HTML uses a limited text-formatting subset rather than browser layout: CSS, scripts, forms, embedded content and document metadata are ignored. Links are inert text and images are alt-text placeholders. No linked images, stylesheets, fonts, media or other resources are loaded, including local and remote URLs. Previewing does not modify the source file.

Choose **Source** for the existing line-numbered text view and Find controls; switching views resets search, selection and scroll. Parsing runs in the background on the same bounded text prefix. Formatting also stops at 32 nested elements, 8,192 parser events, 4,096 text runs, 1,024 blocks or 64 KiB of output, with an explicit complexity notice. Malformed or complex documents may have simplified or incomplete formatting; Source still shows the bounded input prefix. This is not a full browser, CSS renderer or implementation of every Markdown extension.

Select local items and use **Properties** or `Alt+Enter` to calculate their combined logical size and counts in the background. Hidden entries are included; overlapping selected trees are counted once. Symlinks are counted separately without following targets. Hard links are counted per path. Counts update as directory entries are read; cancellation keeps the partial result. The result is logical file size, not disk usage.

Properties show available dates and permissions for selected roots. Unix permissions, UID, and GID do not represent ACLs or effective access; Windows shows read-only state. Properties are read-only and are not available for SFTP. While counting, **Stop calculation** receives initial keyboard focus; press `Space` or `Enter` to stop. You can also close the window or press `Esc` to request cancellation. Results can be partial when files disappear or access fails. Cancellation does not interrupt a blocked OS call.

## Search and Zed

`Cmd/Ctrl+F` filters names in the current folder. `Cmd/Ctrl+Shift+F` opens search. **Folder and subfolders** walks the local folder open when you start Search. Enter a name fragment or a wildcard such as `*.rs`. Comma-separated exclusion masks skip matching names and prune matching directories. Folder search skips symlink branches and returns up to 1,000 results. It reports incomplete coverage if it cannot read a branch or reaches the built-in traversal limit. You can cancel the search and reveal a result in its parent folder.

**Locate index** uses `locate` or `plocate`, with a 15-second wait limit for the query. After a search, PawCommander shows the database paths it can determine and their file modification times as freshness estimates. The index can omit restricted folders and contain stale paths. When you try to reveal a missing result, PawCommander removes it from the current list and keeps the search open. Check **Settings → Tools** to see detected tools. On Windows, compatible command-line tools must be available in `PATH`.

In **Folder and subfolders**, choose **File text** for a literal query or **Regular expression** for a regex. Matching runs line by line and is case-sensitive by default. Results show the path, line number, and a text excerpt. Search skips files over 1 MiB, files containing a NUL byte, and files with invalid UTF-8. Regex search also skips lines over 64 KiB. The result list holds up to 1,000 matching lines.

Open **Filters** to combine entry type, extension, size, modification date, and depth with your query and exclusions. Size and extension apply to regular files. Size bounds are inclusive and share the selected unit. Enter dates as `YYYY-MM-DD` in local time. The **Modified through** date includes the whole day. Set a maximum depth from `1` to `64`, where `1` searches the starting folder's immediate children. You can leave the **File name** query empty when a filter is set.

Choose **Save search…** and enter a name to reuse the query, scope, exclusions, and filters after restarting PawCommander. Select the name under **Saved searches** to run it again. Folder searches scan the saved folder even when a pane is open elsewhere. A saved Locate search queries the currently configured index. Use the **…** menu to rename or remove a saved search.

Choose **Show in results pane** to keep the current results visible while you browse other folders. Select a result to open it or show it in the active pane. Select one or more files and choose **Copy to the other pane** to copy to the displayed destination. Multiple matching lines from a file select it once. If a selected path has disappeared, its rows are removed. Review the remaining selection before retrying a copy.

**Open in Zed** sends a local path or an SSH URL to Zed. Normal opening of a remote file uses a bounded content check to recognize text (UTF-8 or BOM-marked UTF-16); explicitly choosing Zed bypasses that check. Other remote files must be copied locally before opening in external apps.

Zed establishes its own SSH connection using its SSH configuration and authentication. PawCommander's stored secrets are not passed to Zed. A server must allow full SSH access and execution of Zed's helper; SFTP-only access is insufficient. Zed may install its remote server under `~/.zed_server`. Edits are saved on the server; the visible folder updates automatically, or use `F5` to refresh immediately. See [Zed Remote Development](https://zed.dev/docs/remote-development).

## Accessibility

Open **Settings → Accessibility** to choose an interface scale from 90% to 150% or enable high contrast. Both choices are saved. `Cmd/Ctrl+=` and `Cmd/Ctrl+-` change the scale, and `Cmd/Ctrl+0` resets it to 100%.

Press `F10` to focus Settings on the toolbar. `Tab` and `Shift+Tab` move between controls, and `Enter` or `Space` activates the focused button. In the file area, Up/Down selects and focuses files, and `Shift` with Up/Down extends the selection. `Shift+Space` toggles selection of the focused row, and `Enter` opens it. The default `Tab` binding switches panes from the file area when both panes are visible.

List rows and grid tiles expose their full names, types, and selection states to screen readers. Screen reader activation toggles a file's selection. Keyboard focus can move to a file without changing the selection.

## Keyboard shortcuts

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
| Search local folders or indexed paths | `Cmd/Ctrl+Shift+F` |
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
