use super::text;
use crate::file_error::{
    AccessAction, AccessCause, AccessError, ArchiveError, ArchiveMemberError, BatchRenameError,
    ChecksumError, ConflictError, ConnectionError, CopyVerificationError, CrossMoveError,
    DescendantProblem, Failure, HostKeyError, KnownHostEntryError, KnownHostsError, ListingError,
    ListingErrorKind, LocalPathAction, ManifestLineError, MetadataError, MetadataField,
    MetadataWarning, MoveHistoryError, MutationError, PathError, PermissionError, PinError,
    PreviewError, ProbeAction, PublicationError, RecoveryNotice, ReplacementRefusal, RetryError,
    RetryProblem, ScanError, SearchError, SpaceError, SshStage, StagingCleanupError, StagingError,
    StagingRecordAction, TransferPlanError, UndoError,
};
#[cfg(target_os = "macos")]
use crate::file_error::{
    CompatiblePlanError, FilePromiseError, FileTemplateError, RemoteEditError, ResumeCheckError,
    ResumeCheckProblem, ResumeRecordEvent, ResumeRole, SyncError,
};
use crate::file_error::{
    RecoveryRecordAction, RecoveryRecordError, RecoveryRecordProblem, RecoveryRecordSection,
};
use crate::operations::TransferError;
use crate::result_history::{HistoryError, HistoryOpenProblem, HistorySaveProblem};
use crate::vfs::{DeletionFailure, Location, TransferFailure};

pub(crate) fn file_error(error: &TransferError) -> String {
    match error {
        TransferError::Failed(reason) => file_failure(reason, false),
        TransferError::PublicationUnconfirmed(reason) => file_failure(reason, true),
        TransferError::Cancelled => crate::tr_format!("Operation cancelled"),
    }
}

fn file_failure(reason: &Failure, unconfirmed: bool) -> String {
    match reason {
        Failure::Path(error) => path_error(error),
        Failure::Connection(error) => connection_error(error),
        Failure::Access(error) => access_error(error),
        Failure::Preview(error) => preview_error(error),
        Failure::Search(error) => search_error(error),
        Failure::Scan(error) => scan_error(error),
        Failure::Checksum(error) => checksum_error(error),
        Failure::CopyVerification(error) => copy_verification_error(error),
        Failure::Archive(error) => archive_error(error),
        Failure::Metadata(error) => metadata_error(error),
        Failure::Space(error) => space_error(error),
        #[cfg(target_os = "macos")]
        Failure::Sync(error) => sync_error(error),
        Failure::Permissions(reason) => permission_error(reason, unconfirmed),
        Failure::ComparisonPane { right, cause } => {
            let pane = if *right {
                text!("Правая панель")
            } else {
                text!("Левая панель")
            };
            format!("{pane} · {}", file_error(cause))
        }
        Failure::Listing(error) => listing_error(error),
        Failure::ConflictClosed => text!("Диалог конфликта закрыт").into(),
        Failure::Conflict(error) => conflict_error(error),
        Failure::BatchRename(error) => batch_rename_error(error),
        Failure::CrossMove(error) => cross_move_error(error),
        Failure::MoveHistory(error) => move_history_error(error),
        #[cfg(target_os = "macos")]
        Failure::RemoteEdit(error) => remote_edit_error(error),
        Failure::Undo(error) => undo_error(error),
        #[cfg(target_os = "macos")]
        Failure::FilePromise(error) => file_promise_error(error),
        #[cfg(target_os = "macos")]
        Failure::FileTemplate(error) => file_template_error(error),
        #[cfg(target_os = "macos")]
        Failure::CompatiblePlan(error) => compatible_plan_error(error),
        Failure::Mutation(reason) => mutation_error(reason, unconfirmed),
        Failure::Publication(reason) => publication_error(reason),
        Failure::Staging(error) => staging_error(error),
        #[cfg(target_os = "macos")]
        Failure::ResumeCheck(error) => resume_check_error(error),
        Failure::Retry(error) => retry_error(error),
        Failure::TransferPlan(error) => transfer_plan_error(error),
        Failure::RecoveryRecord(error) => recovery_record_error(error),
        Failure::StagingCleanup(error) => staging_cleanup_error(error),
        _ => reason.to_string(),
    }
}

fn copy_verification_error(error: &CopyVerificationError) -> String {
    use CopyVerificationError::*;
    match error {
        SourceSize {
            source,
            expected,
            actual,
        } => crate::tr_format!(
            "Read {actual} bytes while copying {source}. Expected {expected}.",
            source = source,
            expected = expected,
            actual = actual
        ),
        CopySize {
            target,
            expected,
            actual,
        } => crate::tr_format!(
            "Read {actual} bytes while verifying {target}. Expected {expected}.",
            target = target,
            expected = expected,
            actual = actual
        ),
        ExtraData { target, expected } => crate::tr_format!(
            "Verification found data beyond {expected} bytes at {target}.",
            target = target,
            expected = expected
        ),
        DigestMismatch(target) => crate::tr_format!(
            "SHA-256 verification failed for {target}. The contents differ.",
            target = target
        ),
        StagedNotRegular { staging, target } => crate::tr_format!(
            "Expected a regular temporary file at {staging} for {target}.",
            staging = staging,
            target = target
        ),
    }
}

fn move_history_error(error: &MoveHistoryError) -> String {
    match error {
        MoveHistoryError::Coverage { target, cause } => crate::tr_format!(
            "Moved to {target}, but per-item history is incomplete.\n{error}",
            target = target,
            error = file_error(cause)
        ),
        MoveHistoryError::Descendants {
            target,
            unconfirmed,
            first,
        } => crate::tr_format!(
            "Moved to {target}. Nested items with unconfirmed moves: {count}.\n{error}",
            target = target,
            count = unconfirmed,
            error = file_error(first)
        ),
        MoveHistoryError::Descendant {
            source,
            target,
            problem,
        } => {
            let reason = match problem {
                DescendantProblem::Missing => {
                    text!("Вложенный объект отсутствует в месте назначения.").into()
                }
                DescendantProblem::Changed => {
                    text!("Вложенный объект в месте назначения изменился.").into()
                }
                DescendantProblem::Unmapped => {
                    text!("Не удалось сопоставить вложенный путь с папкой назначения.").into()
                }
                DescendantProblem::Read(cause) => file_error(cause),
            };
            crate::tr_format!(
                "Could not confirm the move of {source} to {target}. {error}",
                source = source,
                target = target,
                error = reason
            )
        }
    }
}

fn cross_move_error(error: &CrossMoveError) -> String {
    use CrossMoveError::*;

    match error {
        Changed(path) => crate::tr_format!(
            "Move stopped: item {path} changed or is inaccessible",
            path = path
        ),
        UnsupportedType(path) => crate::tr_format!(
            "Safe cross-volume move is unavailable for this object type: {path}",
            path = path
        ),
        #[cfg(target_os = "macos")]
        Smb(path) => format!(
            "{} · {path}",
            text!("Для передачи файлов через SMB используйте копирование.")
        ),
        #[cfg(target_os = "macos")]
        AccessTimes {
            source,
            expected,
            actual,
        } => crate::tr_format!(
            "Saved access times for {source} have {actual} entries. Expected {expected}.",
            source = source,
            expected = expected,
            actual = actual
        ),
        Hold {
            original,
            held,
            cause,
        } => crate::tr_format!(
            "Check the original source {source} and temporary path {held}.\n{error}",
            source = original,
            held = held,
            error = file_error(cause)
        ),
        Restore {
            original,
            held,
            cause,
            restore,
        } => {
            let restoration = if matches!(restore, TransferError::PublicationUnconfirmed(_)) {
                crate::tr_format!(
                    "Restoration from {held} to {source} is unconfirmed. Check both paths.\n{error}",
                    held = held, source = original, error = file_error(restore)
                )
            } else {
                crate::tr_format!(
                    "Could not restore the source from {held} to {source}.\n{error}",
                    held = held,
                    source = original,
                    error = file_error(restore)
                )
            };
            format!("{}\n{restoration}", file_error(cause))
        }
    }
}

fn undo_error(error: &UndoError) -> String {
    use UndoError::*;

    match error {
        OriginalOccupied { target, source } => {
            let message =
                crate::tr_format!("The original name {target} is occupied.", target = target);
            match source {
                Some(source) => format!("{message} ({source})"),
                None => message,
            }
        }
        FolderChanged(path) => {
            crate::tr_format!("A folder required for Undo changed at {path}.", path = path)
        }
        ItemMissing(path) => {
            crate::tr_format!("The item to restore is missing at {path}.", path = path)
        }
        ItemChanged(path) => {
            crate::tr_format!("The item to restore changed at {path}.", path = path)
        }
        RestoredItemMismatch(path) => {
            crate::tr_format!("The restored item does not match at {path}.", path = path)
        }
        Unconfirmed { from, to, cause } => crate::tr_format!(
            "Could not confirm restoration from {source} to {target}. {error}",
            source = from,
            target = to,
            error = file_error(cause)
        ),
        Incomplete {
            restored,
            remaining,
            cause,
        } => crate::tr_format!(
            "Restored: {restored}. Available for another attempt: {remaining}.\n{error}",
            restored = restored,
            remaining = remaining,
            error = file_error(cause)
        ),
    }
}

fn batch_rename_error(error: &BatchRenameError) -> String {
    use BatchRenameError::*;

    match error {
        SelectionCount => text!("Выберите от 2 до 100 000 элементов.").into(),
        SelectionFolder => {
            text!("Для массового переименования выберите элементы в одной папке.").into()
        }
        RuleLength => text!("Шаблон или регулярное выражение слишком длинные.").into(),
        InvalidTemplate => {
            text!("В шаблоне допустимы {stem}, {ext} и {n}. Скобки можно удвоить.").into()
        }
        InvalidRegex(error) => {
            crate::tr_format!("Invalid regular expression: {error}", error = error)
        }
        NameEncoding => text!("Массовое переименование требует имена в UTF-8.").into(),
        NameLength => text!("Новое имя слишком длинное.").into(),
        InvalidName(_) => text!("Новое имя должно быть допустимым именем файла или папки.").into(),
        DuplicateName => text!("Несколько элементов получают совпадающее имя.").into(),
        PreviewRequired => text!("Сначала проверьте новые имена.").into(),
        Changed(path) => crate::tr_format!(
            "An item changed or disappeared after selection: {source}",
            source = path
        ),
        Occupied { target, source } => {
            let message = crate::tr_format!("Name already exists: {target}", target = target);
            match source {
                Some(source) => format!("{message} ({source})"),
                None => message,
            }
        }
        UndoLimit(path) => crate::tr_format!(
            "The rename of {source} exceeds the safe undo limit. Select fewer items.",
            source = path
        ),
        Unverified { from, to, cause } => crate::tr_format!(
            "Could not verify the rename from {source} to {target}. {error}",
            source = from,
            target = to,
            error = file_error(cause)
        ),
        Incomplete {
            completed,
            total,
            cause,
        } => crate::tr_format!(
            "Renamed: {completed} of {total}. {error}",
            completed = completed,
            total = total,
            error = file_error(cause)
        ),
    }
}

#[cfg(target_os = "macos")]
fn compatible_plan_error(error: &CompatiblePlanError) -> String {
    use CompatiblePlanError::*;

    match error {
        EmptySelection => text!("Для совместимого переноса выберите объекты в двух локальных папках").into(),
        NonLocal(path) => crate::tr_format!(
            "Compatible transfer requires local files and folders. Check {path}.", path = path
        ),
        SmbVolume(path) => crate::tr_format!("Compatible transfer is unavailable on the SMB volume at {path}.", path = path),
        ReadOnlyDestination(path) => crate::tr_format!("The destination volume at {path} is read-only.", path = path),
        ReadOnlySource(path) => crate::tr_format!(
            "The source volume at {path} is read-only. Choose Copy with compatible names.", path = path
        ),
        FatSource(path) => crate::tr_format!(
            "For the FAT or exFAT source at {path}, choose Copy with compatible names.", path = path
        ),
        NestedVolume(path) => crate::tr_format!(
            "Compatible transfer cannot cross the volume mounted at {path} inside the selection.", path = path
        ),
        UnsupportedItem(source) => crate::tr_format!(
            "Compatible transfer supports regular files and folders. Check {source}.", source = source
        ),
        StagingPathLimit { destination, limit } => crate::tr_format!(
            "A temporary copy in {destination} would exceed the {limit}-byte path limit. Choose a folder closer to the root.",
            destination = destination, limit = super::display::integer(*limit)
        ),
        NameLimit { target, available } => crate::tr_format!(
            "No compatible name fits at {target}. Only {available} bytes are available for its name.",
            target = target, available = super::display::integer(*available)
        ),
        EntryLimit { source, limit } => crate::tr_format!(
            "The compatible-name plan exceeded {limit} entries while checking {source}. Select fewer items.",
            limit = super::display::integer(*limit), source = source
        ),
        PathBudget { source, limit } => crate::tr_format!(
            "The compatible-name plan exceeded {limit} bytes of paths while checking {source}. Select fewer items.",
            limit = super::display::integer(*limit), source = source
        ),
    }
}

#[cfg(target_os = "macos")]
fn remote_edit_error(error: &RemoteEditError) -> String {
    match error {
        RemoteEditError::SftpRequired(path) => {
            crate::tr_format!("Expected an SFTP source at {path}.", path = path)
        }
        RemoteEditError::WorkingCopyMissing(path) => crate::tr_format!(
            "The working copy is missing at {path}.",
            path = path.display()
        ),
        RemoteEditError::RevisionChanged(path) => crate::tr_format!(
            "The server file at {path} changed or is unavailable.",
            path = path
        ),
        RemoteEditError::MissingAccessMetadata(path) => crate::tr_format!(
            "The server did not report the owner or permissions of {path}.",
            path = path
        ),
        RemoteEditError::UnsupportedMode(path) => crate::tr_format!(
            "The server file at {path} has special permissions that cannot be preserved.",
            path = path
        ),
        RemoteEditError::AccessMismatch(path) => crate::tr_format!(
            "Could not verify ownership or permissions of the temporary file at {path}.",
            path = path
        ),
    }
}

#[cfg(target_os = "macos")]
fn file_template_error(error: &FileTemplateError) -> String {
    use FileTemplateError::*;

    match error {
        UnrepresentableCharacters(encoding) => crate::tr_format!(
            "Some characters cannot be represented in {encoding}. Choose a different encoding.",
            encoding = encoding.label()
        ),
        InvalidSavedPayload(_) => text!("Не удалось прочитать сохранённый шаблон").into(),
        SourceMissing(path) => {
            crate::tr_format!("The template source file {path} is missing.", path = path)
        }
        TooLarge(Some(path)) => crate::tr_format!(
            "The template source file {path} exceeds 4 MiB.",
            path = path
        ),
        TooLarge(None) => text!("Шаблон файла превышает 4 МиБ").into(),
    }
}

#[cfg(target_os = "macos")]
fn file_promise_error(error: &FilePromiseError) -> String {
    use FilePromiseError::*;

    match error {
        RemoteSourceRequired(path) => {
            crate::tr_format!("Expected an SFTP source at {path}.", path = path)
        }
        SourceMissing(path) => {
            crate::tr_format!("The dragged item no longer exists at {path}.", path = path)
        }
        SourceTypeChanged(path) => crate::tr_format!(
            "The dragged item changed type at {path}. Refresh the folder and drag it again.",
            path = path
        ),
        InvalidReceiverUrl => text!("Принимающее приложение не указало путь к файлу.").into(),
        InvalidDestination(path) => crate::tr_format!(
            "Invalid destination provided by the receiving application: {path}.",
            path = path.display()
        ),
        QueueUnavailable => text!("Очередь файловых операций недоступна.").into(),
        QueueDisconnected => text!("Очередь закрылась до подтверждения передачи файла.").into(),
    }
}

pub(crate) fn replacement_refusal(reason: ReplacementRefusal) -> &'static str {
    match reason {
        ReplacementRefusal::SourceNotRegular => {
            text!("Для замены исходный объект должен быть обычным файлом.")
        }
        ReplacementRefusal::TargetNotRegular => {
            text!("Для замены объект назначения должен быть обычным файлом.")
        }
        ReplacementRefusal::SameFile => text!("Оба пути ведут к одному файлу."),
        ReplacementRefusal::WouldOverwriteArchive => {
            text!("Замена затронет архив, из которого извлекаются файлы.")
        }
        #[cfg(target_os = "macos")]
        ReplacementRefusal::Smb => {
            text!("Безопасная замена файла на SMB недоступна. Выберите свободное имя.")
        }
    }
}

fn conflict_error(error: &ConflictError) -> String {
    match error {
        ConflictError::ReplacementUnavailable {
            source,
            target,
            reason,
        } => crate::tr_format!(
            "Could not replace {target} with {source}. {reason}",
            source = source,
            target = target,
            reason = replacement_refusal(*reason)
        ),
        ConflictError::SourceChanged(source) => crate::tr_format!(
            "Source item {source} changed after the conflict dialog was shown.",
            source = source
        ),
    }
}

fn transfer_plan_error(error: &TransferPlanError) -> String {
    use TransferPlanError::*;

    let name_at = |location: &Location| match (location.parent(), location.path().file_name()) {
        (Some(parent), Some(name)) => format!("{parent} · {name:?}"),
        _ => format!("{:?}", location.path()),
    };
    match error {
        MissingName(path) => crate::tr_format!("Path {path} has no file name.", path = path),
        MissingParent(path) => crate::tr_format!("Path {path} has no parent folder.", path = path),
        NonUtf8Name(path) => crate::tr_format!(
            "SFTP transfer requires a UTF-8 name. {path}",
            path = name_at(path)
        ),
        InvalidSourceName { location, cause } => crate::tr_format!(
            "SFTP transfer cannot use this name. {path}. {error}",
            path = name_at(location),
            error = path_error(cause)
        ),
        DuplicateTarget { first, second, target } => crate::tr_format!(
            "Selected items {first} and {second} have the same destination {target}. Transfer them separately.",
            first = first,
            second = second,
            target = target
        ),
        SameFolder { source, destination } => crate::tr_format!(
            "Item {source} is already in the destination folder {destination}.",
            source = source,
            destination = destination
        ),
        IntoSource { source, destination } => crate::tr_format!(
            "Destination {destination} is the source {source} or inside it.",
            destination = destination,
            source = source
        ),
        NoUnusedName(target) => crate::tr_format!(
            "Could not choose an unused name for {target}",
            target = target
        ),
    }
}

fn retry_error(error: &RetryError) -> String {
    use RetryProblem::*;

    let path = &error.location;
    match error.problem {
        InspectionRequired => crate::tr_format!(
            "Inspect the interrupted SFTP transfer to {path} before retrying.",
            path = path
        ),
        RequestChanged => crate::tr_format!(
            "The selection or operation for the retry to {path} changed. Review the selection.",
            path = path
        ),
        SourceChanged => crate::tr_format!(
            "Source {path} changed or is unavailable. Inspect it before retrying.",
            path = path
        ),
        DestinationChanged => crate::tr_format!(
            "Destination folder {path} changed or is unavailable. Select it again.",
            path = path
        ),
        TargetChanged => crate::tr_format!(
            "Destination item {path} changed since the previous attempt. Inspect it before retrying.",
            path = path
        ),
        BackupRetained => crate::tr_format!(
            "A backup of the previous destination remains at {path}. Inspect it before retrying.",
            path = path
        ),
        OriginalRetained => crate::tr_format!(
            "The original item remains in temporary storage at {path}. Inspect it before retrying.",
            path = path
        ),
        PayloadMissing => crate::tr_format!(
            "The temporary copy needed for the retry is missing at {path}.",
            path = path
        ),
        PayloadPathMissing => crate::tr_format!(
            "The previous attempt to {path} did not record a path to the required temporary copy.",
            path = path
        ),
    }
}

#[cfg(target_os = "macos")]
fn resume_check_error(error: &ResumeCheckError) -> String {
    use ResumeCheckProblem::*;

    let role = match error.role {
        ResumeRole::Source => text!("Исходный объект"),
        ResumeRole::Destination => text!("Объект назначения"),
        ResumeRole::Staged => text!("Временная копия"),
        ResumeRole::ParkedDestination => text!("Сохранённое прежнее назначение"),
        ResumeRole::HeldOriginal => text!("Исходник во временной папке"),
    };
    let detail = match error.problem {
        Missing => text!("Объект отсутствует."),
        Unexpected => text!("На этом пути появился неожиданный объект."),
        Changed => text!("Объект изменился с момента проверки."),
        ContentsMismatch => text!("Содержимое отличается от проверенных данных."),
        WrongType => text!("Неподходящий тип объекта."),
        IncompleteMetadata => text!("Для сверки не хватает метаданных."),
        InvalidPath => text!("Недопустимый путь для сверки."),
        MissingProof => text!("В записи восстановления отсутствуют данные для сверки объекта."),
        SizeMismatch { expected, actual } => {
            let size = crate::tr_format!(
                "Found {actual} bytes while verifying, expected {expected}.",
                actual = actual,
                expected = expected
            );
            return format!("{role} «{}». {size}", error.location);
        }
        ExtraData { expected } => {
            let size = crate::tr_format!(
                "Found more than {expected} bytes while verifying.",
                expected = expected
            );
            return format!("{role} «{}». {size}", error.location);
        }
    };
    format!("{role} «{}». {detail}", error.location)
}

pub(crate) fn recovery_record_error(error: &RecoveryRecordError) -> String {
    use RecoveryRecordProblem::*;

    let path = error.path.display();
    match &error.problem {
        Read(error) => crate::tr_format!(
            "Could not read recovery record {path}: {error}", path = path, error = error
        ),
        ReadDirectory(error) => crate::tr_format!(
            "Could not read recovery folder {path}: {error}", path = path, error = error
        ),
        ScanStart(error) => crate::tr_format!(
            "Could not start recovery lookup in {path} ({error}).", path = path, error = error
        ),
        ScanDisconnected => crate::tr_format!(
            "Lost contact with the recovery worker for {path}.", path = path
        ),
        Lock(error) => crate::tr_format!(
            "Could not lock recovery record {path}: {error}", path = path, error = error
        ),
        Write { action, source } => {
            let action = recovery_record_action(*action);
            format!("{action} «{path}» ({source})")
        }
        Encode {
            #[cfg(target_os = "macos")]
            event,
            source,
        } => {
            let data = text!("исходные данные записи");
            #[cfg(target_os = "macos")]
            let data = event.map_or(data, resume_record_event);
            crate::tr_format!(
                "Could not encode {data} for recovery record {path} ({error})",
                data = data, path = path, error = source
            )
        }
        #[cfg(target_os = "macos")]
        AppendRollback { event, source, restore } => {
            let action = recovery_record_action(RecoveryRecordAction::Append(*event));
            let followup = crate::tr_format!(
                "Could not restore the previous length of the recovery record ({error}).",
                error = restore
            );
            format!("{action} «{path}» ({source})\n{followup}")
        }
        InUse => format!("{} · {path}", text!("Эта копия уже обрабатывается другим заданием.")),
        AlreadyActive => crate::tr_format!(
            "The worker already has an active recovery record. Could not activate {path}.", path = path
        ),
        MissingParent => crate::tr_format!(
            "Recovery record {path} has no containing directory.", path = path
        ),
        InvalidType => crate::tr_format!(
            "Expected a regular file for recovery record {path}.", path = path
        ),
        IdentityChanged => crate::tr_format!(
            "Recovery record {path} was replaced or is no longer a regular file", path = path
        ),
        SizeLimit(limit) => crate::tr_format!(
            "Recovery record {path} exceeds the {limit}-byte limit", path = path, limit = limit
        ),
        Json { section: RecoveryRecordSection::Header, source } => crate::tr_format!(
            "Could not decode the header of recovery record {path}: {error}", path = path, error = source
        ),
        Json { section: RecoveryRecordSection::Event(number), source } => crate::tr_format!(
            "Could not decode event {number} in recovery record {path}: {error}",
            number = number, path = path, error = source
        ),
        #[cfg(target_os = "macos")]
        Json { section: RecoveryRecordSection::AccessTimes, source } => format!(
            "{} · {path} ({source})", text!("Некорректная запись времени доступа для переноса")
        ),
        UnsupportedVersion(version) => crate::tr_format!(
            "Unsupported version {version} of recovery record {path}", version = version, path = path
        ),
        IncompleteEvent(number) => crate::tr_format!(
            "Event {number} in recovery record {path} is incomplete.", number = number, path = path
        ),
        OmittedInvalidEvents(count) => crate::tr_format!(
            "Additional invalid events in recovery record {path} ({count}).", path = path, count = count
        ),
        #[cfg(target_os = "macos")]
        VersionMismatch { stored, expected } => crate::tr_format!(
            "Recovery record {path} has version {stored} but its contents require version {expected}",
            path = path, stored = stored, expected = expected
        ),
        #[cfg(target_os = "macos")]
        EventOrder(number) => crate::tr_format!(
            "Event {number} is out of order in recovery record {path}", path = path, number = number
        ),
        #[cfg(target_os = "macos")]
        InvalidAccessTimes => format!("{} · {path}", text!("Некорректная запись времени доступа для переноса")),
        #[cfg(target_os = "macos")]
        InvalidPaths => crate::tr_format!("Invalid paths in recovery record {path}", path = path),
        #[cfg(target_os = "macos")]
        InvalidProof => crate::tr_format!("Invalid transfer proof in recovery record {path}", path = path),
        #[cfg(target_os = "macos")]
        InvalidName => crate::tr_format!("Invalid recovery record name in {path}", path = path),
        ScanLimit(limit) => crate::tr_format!(
            "Stopped reading recovery folder {path} after {limit} entries", path = path, limit = limit
        ),
        ReportLimit(limit) => crate::tr_format!(
            "Recovery report for {path} reached the {limit}-byte limit. More records may exist.",
            path = path, limit = limit
        ),
    }
}

#[cfg(target_os = "macos")]
fn resume_record_event(event: ResumeRecordEvent) -> &'static str {
    match event {
        ResumeRecordEvent::Publication => text!("идентификатор проверенной копии"),
        ResumeRecordEvent::HoldingSource => text!("временный путь исходника"),
        ResumeRecordEvent::SourceRemoved => text!("подтверждение удаления исходника"),
    }
}

fn recovery_record_action(action: RecoveryRecordAction) -> String {
    use RecoveryRecordAction::*;
    match action {
        CreateDirectory => text!("Не удалось создать папку записей восстановления"),
        CreateFile => text!("Не удалось создать временную запись восстановления в папке"),
        WriteInitial => text!("Не удалось записать начальные данные восстановления"),
        SyncInitial => text!("Не удалось сохранить начальные данные восстановления на диск"),
        Publish => text!("Не удалось разместить запись восстановления"),
        SyncPublication => {
            text!("Не удалось подтвердить сохранение записи восстановления на диске")
        }
        Activate => text!("Не удалось активировать запись восстановления"),
        CheckIdentity => text!("Не удалось проверить подлинность записи восстановления"),
        #[cfg(target_os = "macos")]
        SeekEnd => text!("Не удалось найти позицию для добавления события в запись восстановления"),
        Remove => text!("Не удалось удалить запись восстановления"),
        SyncRemoval => text!("Не удалось подтвердить удаление записи восстановления на диске"),
        #[cfg(target_os = "macos")]
        Append(event) => {
            return crate::tr_format!(
                "Could not append {event} to recovery record",
                event = resume_record_event(event)
            )
        }
        #[cfg(target_os = "macos")]
        SyncEvent(event) => {
            return crate::tr_format!(
                "Could not save {event} to disk for recovery record",
                event = resume_record_event(event)
            )
        }
    }
    .into()
}

pub(crate) fn history_error(error: &HistoryError) -> String {
    match error {
        HistoryError::Unavailable => text!("Журнал результатов недоступен.").into(),
        HistoryError::Disconnected(path) => crate::tr_format!(
            "Lost contact with the result history worker for {path}.",
            path = path.display()
        ),
        HistoryError::Spawn { path, source } => crate::tr_format!(
            "Could not start the result history worker for {path} ({error}).",
            path = path.display(),
            error = source
        ),
        HistoryError::Open { path, problem } => history_open_problem(path, problem),
        HistoryError::Save { path, problem } => history_save_problem(path, problem),
    }
}

fn history_open_problem(path: &std::path::Path, problem: &HistoryOpenProblem) -> String {
    let path = path.display();
    match problem {
        HistoryOpenProblem::MissingParent => {
            crate::tr_format!(
                "Result history {path} has no containing directory.",
                path = path
            )
        }
        HistoryOpenProblem::CreateDirectory(source) => crate::tr_format!(
            "Could not create the result history directory {path} ({error}).",
            path = path,
            error = source
        ),
        HistoryOpenProblem::OpenLock(source) => crate::tr_format!(
            "Could not open the result history lock file {path} ({error}).",
            path = path,
            error = source
        ),
        HistoryOpenProblem::Locked => {
            crate::tr_format!("Result history is in use. Lock file {path}.", path = path)
        }
        HistoryOpenProblem::Lock(source) => crate::tr_format!(
            "Could not lock result history at {path} ({error}).",
            path = path,
            error = source
        ),
        HistoryOpenProblem::Read(source) => crate::tr_format!(
            "Could not read result history {path} ({error}).",
            path = path,
            error = source
        ),
        HistoryOpenProblem::InvalidType => crate::tr_format!(
            "Result history requires a regular file at {path}.",
            path = path
        ),
        HistoryOpenProblem::IdentityChanged => crate::tr_format!(
            "The file {path} changed while opening result history.",
            path = path
        ),
        HistoryOpenProblem::SizeLimit(limit) => crate::tr_format!(
            "Result history {path} exceeds the {limit}-byte limit.",
            path = path,
            limit = limit
        ),
        HistoryOpenProblem::Json(source) => crate::tr_format!(
            "Invalid result history data in {path} ({error}).",
            path = path,
            error = source
        ),
        HistoryOpenProblem::UnsupportedVersion(version) => crate::tr_format!(
            "Unsupported version {version} of result history {path}.",
            version = version,
            path = path
        ),
    }
}

fn history_save_problem(path: &std::path::Path, problem: &HistorySaveProblem) -> String {
    use HistorySaveProblem::*;

    let path = path.display();
    match problem {
        MissingParent => crate::tr_format!(
            "Result history {path} has no containing directory.",
            path = path
        ),
        Serialize(source) => crate::tr_format!(
            "Could not encode result history {path} ({error}).",
            path = path,
            error = source
        ),
        SizeLimit(limit) => crate::tr_format!(
            "Result history {path} exceeds the {limit}-byte limit.",
            path = path,
            limit = limit
        ),
        CreateTemporary(source) => crate::tr_format!(
            "Could not create a temporary file for result history {path} ({error}).",
            path = path,
            error = source
        ),
        WriteTemporary(source) => crate::tr_format!(
            "Could not write the temporary file for result history {path} ({error}).",
            path = path,
            error = source
        ),
        SyncTemporary(source) => crate::tr_format!(
            "Could not sync the temporary file for result history {path} ({error}).",
            path = path,
            error = source
        ),
        CheckLock(source) => crate::tr_format!(
            "Could not verify the result history lock {path} ({error}).",
            path = path,
            error = source
        ),
        LockChanged => crate::tr_format!(
            "The result history lock {path} changed before saving.",
            path = path
        ),
        Replace(source) => crate::tr_format!(
            "Could not replace result history {path} ({error}).",
            path = path,
            error = source
        ),
        ConfirmReplacement(source) => crate::tr_format!(
            "Result history {path} was replaced. Could not confirm the change on disk ({error}).",
            path = path,
            error = source
        ),
    }
}

fn metadata_error(error: &MetadataError) -> String {
    match error {
        MetadataError::TargetChanged(target) => crate::tr_format!(
            "Item {target} changed while preserving metadata.",
            target = target
        ),
        MetadataError::CleanupDirectoryChanged(target) => format!(
            "{} · {target}",
            text!("Временная папка изменилась перед очисткой")
        ),
    }
}

#[cfg(target_os = "macos")]
fn sync_error(error: &SyncError) -> String {
    match error {
        SyncError::NameCollision(location) => crate::tr_format!(
            "Names collide at the destination: {location}", location = location
        ),
        SyncError::RetainedStage(location) => crate::tr_format!(
            "Found a sync staging folder: {location}. Wait for any running synchronization to finish. If it was interrupted, inspect the retained versions before building a new plan.", location = location
        ),
        SyncError::Overlap { source, destination } => format!(
            "{} · {source} → {destination}",
            text!("Источник и назначение синхронизации пересекаются")
        ),
        SyncError::PlanLimit => text!(
            "Достигнут предел плана: 100 000 объектов или 32 МиБ данных о путях"
        ).into(),
        SyncError::ExcludedPath { target, excluded } => crate::tr_format!(
            "Replacing or deleting {location} would affect the excluded path {excluded}.",
            location = target, excluded = excluded.display()
        ),
        SyncError::BaselineChanged => text!("Общее состояние изменилось. Постройте план заново.").into(),
        SyncError::UnresolvedConflicts => text!("Для применения нужен план без нерешённых конфликтов.").into(),
        SyncError::ResultMismatch(folder) => format!(
            "{} · {folder}",
            text!("После применения содержимое папок отличается от плана. Постройте план заново.")
        ),
    }
}

pub(crate) fn metadata_warning(warning: &MetadataWarning) -> String {
    let fields = warning
        .fields
        .iter()
        .map(|field| match field {
            MetadataField::Permissions => text!("права доступа"),
            MetadataField::AccessTime => text!("время доступа"),
            MetadataField::ModificationTime => text!("время изменения"),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut lines = vec![crate::tr_format!(
        "Could not fully preserve {fields}.",
        fields = fields
    )];
    for cause in &warning.causes {
        let cause = cause.to_string();
        if !lines.contains(&cause) {
            lines.push(cause);
        }
    }
    lines.join("\n")
}

pub(crate) fn connection_result_error(error: &TransferError) -> String {
    if matches!(error, TransferError::Cancelled) {
        text!("Подключение отменено").into()
    } else {
        file_error(error)
    }
}

fn space_error(error: &SpaceError) -> String {
    match error {
        SpaceError::MissingDestinationParent(target) => format!(
            "{} · {target}", text!("Недопустимое назначение")
        ),
        SpaceError::LocalQuery { directory, source } => crate::tr_format!(
            "Could not check available space in “{parent}”: {error}",
            parent = directory.display(), error = source
        ),
        SpaceError::LocalWrite { directory, source } => crate::tr_format!(
            "Not enough free space while copying into “{parent}”: {error}",
            parent = directory.display(), error = access_io_message(source)
        ),
        SpaceError::Insufficient { directory, required, available } => crate::tr_format!(
            "Not enough free space in “{parent}”: about {required} bytes needed, {available} bytes available",
            parent = directory, required = required, available = available
        ),
        SpaceError::SftpProbe(failure) => failure.causes.iter().map(|(action, source)| {
            let parent = &failure.directory;
            let probe = &failure.probe;
            let probe_error = |message| format!("{message} «{probe}»: {source}");
            match action {
                ProbeAction::Query if source.code() == ssh2::ErrorCode::SFTP(libssh2_sys::LIBSSH2_FX_OP_UNSUPPORTED) => crate::tr_format!(
                    "Could not check available SFTP space in {parent}. The server does not support fstatvfs@openssh.com ({error}). No new file data was written",
                    parent = parent, error = source
                ),
                ProbeAction::Query => crate::tr_format!(
                    "Could not check available SFTP space in {parent} using fstatvfs@openssh.com: {error}. No new file data was written",
                    parent = parent, error = source
                ),
                ProbeAction::Open => probe_error(text!("Не удалось открыть проверочный файл SFTP")),
                ProbeAction::Close => probe_error(text!("Не удалось закрыть проверочный файл SFTP")),
                ProbeAction::Remove => probe_error(text!("Не удалось удалить проверочный файл SFTP")),
            }
        }).collect::<Vec<_>>().join(". "),
    }
}

fn archive_error(error: &ArchiveError) -> String {
    match error {
        ArchiveError::InvalidName(_) => text!("Введите допустимое имя архива").into(),
        ArchiveError::SelectCurrentFolder => {
            text!("Выберите файлы или папки из текущей папки").into()
        }
        ArchiveError::SelectArchiveEntries => text!("Выберите записи из одной папки архива").into(),
        ArchiveError::SelectOtherPane => text!("Выберите файлы или папки в соседней панели").into(),
        ArchiveError::PasswordRequiresZip => text!("Пароль можно задать только для ZIP").into(),
        ArchiveError::UnsupportedFormat(archive) => {
            format!("{} · {archive}", text!("Поддерживаются архивы ZIP и TAR"))
        }
        ArchiveError::UnsupportedEntry(entry) => crate::tr_format!(
            "Only regular files and folders can be added to an archive: {source}",
            source = entry
        ),
        ArchiveError::MissingEntrySize(entry) => crate::tr_format!(
            "The server did not report the source file size: {source}",
            source = entry
        ),
        ArchiveError::EntryEncoding(entry) => format!(
            "{} · {entry}",
            text!("Для создания архива нужны имена файлов в UTF-8")
        ),
        ArchiveError::MemberMissing(entry) => {
            crate::tr_format!("Archive entry no longer exists: {}", entry)
        }
        ArchiveError::MemberOccupied(entry) => {
            crate::tr_format!("Path is already used in the archive: {}", entry)
        }
        ArchiveError::MemberNameUnchanged(entry) => {
            format!("{} · {entry}", text!("Имя записи архива не изменилось"))
        }
        ArchiveError::CannotAddArchiveToItself(archive) => format!(
            "{} · {archive}",
            text!("Архив нельзя добавить в него самого")
        ),
        ArchiveError::AdditionsRequirePassword(archive) => format!(
            "{} · {archive}",
            text!("Задайте пароль для добавляемых файлов ZIP")
        ),
        ArchiveError::ExtractionLimit(entry) => format!(
            "{} · {entry}",
            text!("Выбрано больше 32 ГиБ для распаковки")
        ),
        ArchiveError::ExtractionDestinationChanged(directory) => crate::tr_format!(
            "The destination folder changed: {directory}",
            directory = directory
        ),
        ArchiveError::ExtractionReplaceUnavailable(destination) => format!(
            "{} · {destination}",
            text!("Замена доступна только для двух разных обычных файлов.")
        ),
        ArchiveError::EntryPath { entry, source } => {
            let reason = match source {
                ArchiveMemberError::InvalidPath => text!("Недопустимый путь внутри архива"),
                ArchiveMemberError::LimitExceeded => {
                    text!("Превышен лимит записей или путей в архиве")
                }
            };
            format!("{reason} · {entry}")
        }
        ArchiveError::AddEntry {
            archive,
            entry,
            source,
        } => crate::tr_format!(
            "Could not add {entry} to archive {archive}: {error}",
            archive = archive,
            entry = entry,
            error = source
        ),
        ArchiveError::Read { archive, source } => crate::tr_format!(
            "Could not read archive “{}”: {error}",
            archive,
            error = source
        ),
        ArchiveError::ExtractEntry {
            entry,
            destination,
            source,
        } => crate::tr_format!(
            "Could not extract {entry} to {destination}: {error}",
            entry = entry,
            destination = destination,
            error = source
        ),
        ArchiveError::Rebuild { location, source } => crate::tr_format!(
            "Could not rebuild archive at {location}: {error}",
            location = location,
            error = source
        ),
        ArchiveError::Verify { archive, source } => format!("{archive}: {source}"),
        ArchiveError::ChangedDuringVerification(archive) => {
            format!("{} · {archive}", text!("Архив изменился во время проверки"))
        }
        ArchiveError::SourceChanged(archive) => {
            crate::tr_format!("The archive changed during the operation: {}", archive)
        }
    }
}

fn connection_error(error: &ConnectionError) -> String {
    match error {
        ConnectionError::InvalidEndpoint => text!("Укажите сервер, пользователя и порт 1–65535").into(),
        ConnectionError::Resolve { host, source } => crate::tr_format!(
            "Could not resolve SFTP host {host}: {error}",
            host = host,
            error = source
        ),
        ConnectionError::Connect { endpoint, last: Some((address, source)) } => crate::tr_format!(
            "Could not connect to the SFTP server {endpoint} ({address}): {error}",
            endpoint = endpoint,
            address = address,
            error = source
        ),
        ConnectionError::Connect { endpoint, last: None } => crate::tr_format!(
            "No addresses found for the SFTP server {endpoint}", endpoint = endpoint
        ),
        ConnectionError::Transport(source) => crate::tr_format!(
            "Could not configure the SSH connection: {error}", error = source
        ),
        ConnectionError::Ssh { stage, source } => {
            let prefix = match stage {
                SshStage::Session => text!("Не удалось создать сеанс SSH"),
                SshStage::Handshake => text!("Не удалось согласовать соединение SSH"),
                SshStage::AuthenticationMethods => text!("Не удалось получить способы SSH-аутентификации"),
                SshStage::Authentication => text!("Сервер не подтвердил аутентификацию"),
                SshStage::InteractiveAuthentication => text!("Сервер не подтвердил интерактивную аутентификацию"),
                SshStage::Channel => text!("Не удалось открыть канал SFTP"),
                SshStage::Agent => return crate::tr_format!(
                    "SSH agent ({error}). Add a key to the agent or supply a key or password in SFTP.",
                    error = source
                ),
            };
            format!("{prefix}: {source}")
        }
        ConnectionError::HostKey(error) => host_key_error(error),
        #[cfg(target_os = "macos")]
        ConnectionError::Forward { target, source } => crate::tr_format!(
            "Could not forward TCP to {address} ({error})",
            address = target,
            error = source
        ),
        ConnectionError::Route { jump, address, cause } => {
            let message = file_error(cause);
            if *jump {
                crate::tr_format!("Jump host {address} · {error}", address = address, error = message)
            } else {
                crate::tr_format!("Target host {address} · {error}", address = address, error = message)
            }
        }
        ConnectionError::Automatic(cause) => crate::tr_format!(
            "Could not open the SFTP connection ({error}). Use the SFTP dialog to sign in with a password or MFA.",
            error = file_error(cause)
        ),
        ConnectionError::AuthenticationUnconfirmed { interactive: true } =>
            text!("Сервер не подтвердил интерактивную аутентификацию").into(),
        ConnectionError::AuthenticationUnconfirmed { interactive: false } =>
            text!("Сервер не подтвердил аутентификацию").into(),
        ConnectionError::UnsupportedAuthentication => text!("Сервер не предложил доступный способ аутентификации").into(),
        ConnectionError::InteractiveTimeout => text!("Время интерактивной SSH-аутентификации истекло").into(),
        ConnectionError::HostKeyTimeout => text!("Время проверки ключа сервера SSH истекло").into(),
        ConnectionError::Finalized => text!("Подключение SSH уже завершено").into(),
    }
}

fn host_key_error(error: &HostKeyError) -> String {
    match error {
        HostKeyError::HomeUnavailable => text!("Не найдена домашняя папка").into(),
        HostKeyError::KeyUnavailable => text!("Сервер не предоставил ключ SSH").into(),
        HostKeyError::Unverified => text!("Ключ сервера SSH не подтверждён. Откройте форму SFTP и проверьте отпечаток.").into(),
        HostKeyError::PinDirectoryUnavailable => text!("Не найдена папка для доверенных ключей PawCommander").into(),
        HostKeyError::KnownHosts { path, cause } => {
            let error = match cause {
                KnownHostsError::Read(source) => source.to_string(),
                KnownHostsError::Initialize(source) => source.to_string(),
                KnownHostsError::Entry { line, cause } => {
                    let error = match cause {
                        KnownHostEntryError::Format => text!("Некорректная запись known_hosts").into(),
                        KnownHostEntryError::Encoding(source) => source.to_string(),
                        KnownHostEntryError::Parse(source) => source.to_string(),
                    };
                    crate::tr_format!("known_hosts line {line}: {error}", line = line, error = error)
                }
                KnownHostsError::Check => text!("Ошибка проверки записи known_hosts").into(),
                KnownHostsError::Revoked => text!("Ключ сервера SSH отозван").into(),
                KnownHostsError::CertificateAuthority => text!("Для этого сервера в known_hosts указан центр сертификации SSH. Проверьте сертификат через OpenSSH.").into(),
            };
            crate::tr_format!(
                "Could not verify the SSH host key using {path} ({error})",
                path = path.display(),
                error = error
            )
        }
        HostKeyError::KnownHostsChanged { path, received } => crate::tr_format!(
            "{changed} The server presented {fingerprint}. Verify the fingerprint with the administrator and review the entry in {path}.",
            changed = text!("Ключ сервера SSH изменился."), fingerprint = received, path = path.display()
        ),
        HostKeyError::Pin { path, cause } => {
            let path = path.display();
            let error = match cause {
                PinError::Read(source) | PinError::Write(source) => source.to_string(),
                PinError::Json(source) => source.to_string(),
                PinError::TooLarge => text!("Запись доверенного ключа слишком велика").into(),
                PinError::Changed { saved, received } => return crate::tr_format!(
                    "{changed} Saved fingerprint {saved}, received {received}. Verify the fingerprint with the administrator and review the record in {path}.",
                    changed = text!("Ключ сервера SSH изменился."), path = path, received = received, saved = saved
                ),
            };
            crate::tr_format!(
                "Could not read or save the trusted SSH key in {path} ({error})",
                path = path,
                error = error
            )
        }
    }
}

fn access_error(error: &AccessError) -> String {
    let cause = match &error.cause {
        AccessCause::Connection(error) => file_error(error),
        #[cfg(target_os = "macos")]
        AccessCause::Path(error) => path_error(error),
        cause => cause.to_string(),
    };
    let action = match error.action {
        AccessAction::ResolvePath => text!("Не удалось определить путь к элементу"),
        AccessAction::ReadMetadata => {
            text!("Не удалось получить сведения об элементе")
        }
        AccessAction::ReadOpenMetadata => {
            text!("Не удалось прочитать метаданные открытого объекта")
        }
        AccessAction::OpenFile => {
            return crate::tr_format!("Could not open {}: {error}", error.location, error = cause);
        }
        AccessAction::CreateFile => text!("Не удалось создать файл"),
        AccessAction::CreateDirectory => text!("Не удалось создать папку"),
        AccessAction::CreateStagingDirectory => {
            text!("Не удалось создать временную папку для копирования")
        }
        AccessAction::RemoveStagingDirectory => text!("Не удалось удалить временную папку"),
        #[cfg(target_os = "macos")]
        AccessAction::RemoveStagingItem => text!("Не удалось удалить временный элемент"),
        AccessAction::ReadChildren => text!("Не удалось прочитать папку"),
        AccessAction::ReadFile => text!("Не удалось прочитать файл"),
        AccessAction::ReadLink => text!("Не удалось прочитать символическую ссылку"),
        AccessAction::CreateLink => text!("Не удалось создать символическую ссылку"),
        AccessAction::WriteFile => text!("Не удалось записать файл"),
        AccessAction::FlushFile => text!("Не удалось записать буфер файла"),
        AccessAction::SyncFile => text!("Не удалось сохранить данные файла"),
        AccessAction::CloseFile => text!("Не удалось закрыть файл"),
        AccessAction::SeekFile => text!("Не удалось перейти к нужной позиции в файле"),
        AccessAction::WriteMetadata => text!("Не удалось сохранить метаданные объекта"),
        AccessAction::WritePermissions => text!("Не удалось применить права доступа"),
        #[cfg(target_os = "macos")]
        AccessAction::CheckSmbVolume => text!("Не удалось проверить SMB-том назначения"),
        #[cfg(target_os = "macos")]
        AccessAction::ReadVolumeLimits => text!("Не удалось определить ограничения тома"),
        AccessAction::CheckSearchResult => {
            return crate::tr_format!(
                "Could not check search result {path}: {error}",
                path = error.location,
                error = cause
            );
        }
        #[cfg(target_os = "macos")]
        AccessAction::WritePreviewCache => {
            text!("Не удалось записать временный файл предпросмотра")
        }
    };
    format!("{action} «{}»: {cause}", error.location)
}

pub(crate) fn access_io_message(error: &std::io::Error) -> String {
    let Some(cause) = error.get_ref() else {
        return error.to_string();
    };
    if let Some(reason) = cause.downcast_ref::<Failure>() {
        return file_failure(reason, false);
    }
    if let Some(error) = cause.downcast_ref::<AccessError>() {
        return access_error(error);
    }
    error.to_string()
}

fn search_error(error: &SearchError) -> String {
    let (message, location) = match error {
        SearchError::RequiresSftpConnection(location) => (
            text!("Для поиска подключитесь к серверу по SFTP."),
            location,
        ),
        SearchError::DifferentSftpConnection(location) => (
            text!("Результаты относятся к разным подключениям SFTP."),
            location,
        ),
        SearchError::DirectoryChanged { location, resolved } => {
            return crate::tr_format!(
                "The SFTP folder {path} now resolves to {target}. It was skipped.",
                path = location,
                target = resolved.display()
            );
        }
    };
    format!("{message} · {location}")
}

fn scan_error(error: &ScanError) -> String {
    let (message, location) = match error {
        ScanError::PropertiesStart(source) => {
            return format!("{}: {source}", text!("Подсчёт не запущен"));
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        ScanError::DirectoryChanged(location) => {
            (text!("Папка изменилась во время подсчёта"), location)
        }
        ScanError::InvalidSftpEntry { directory, name } => {
            return format!(
                "{} · {directory} · {name:?}",
                text!("Сервер SFTP вернул недопустимое имя")
            );
        }
        ScanError::DepthLimit(location) => (text!("Достигнут предел вложенности папок"), location),
        ScanError::TransferDepthLimit { location, limit } => {
            return crate::tr_format!(
                "Folders are nested more than {limit} levels at {path}.",
                limit = limit,
                path = location,
            )
        }
        ScanError::MoveEntryLimit(location) => (
            text!("Пофайловый журнал переноса превысил лимит 100000 элементов"),
            location,
        ),
        ScanError::TransferSizeOverflow(location) => {
            (text!("Слишком большой размер копируемого дерева"), location)
        }
        ScanError::FolderUnavailable(location) => {
            (text!("Папка изменилась или недоступна"), location)
        }
        ScanError::DuplicateEntry { directory, name } => {
            return format!(
                "{} · {directory}/{name:?}",
                text!("Папка содержит повторяющиеся имена")
            );
        }
        ScanError::UnsupportedComparisonType(location) => {
            (text!("Тип объекта недоступен для сравнения"), location)
        }
        ScanError::MissingComparisonMetadata(location) => (
            text!("Не удалось получить размер или время изменения"),
            location,
        ),
        ScanError::ComparisonLimit => {
            return text!("Достигнут предел сравнения: 100 000 объектов или 32 МиБ данных о путях",)
                .into()
        }
        ScanError::FileChanged(location) => (text!("Файл изменился во время чтения"), location),
        ScanError::ExpectedRegularFile(location) => (
            text!("Для проверки подходят только обычные файлы без ссылок"),
            location,
        ),
        #[cfg(target_os = "macos")]
        ScanError::MissingLocalFileSize => return text!("Размер файла недоступен").into(),
        ScanError::MissingSftpFileSize(location) => {
            (text!("Сервер SFTP не сообщил размер файла"), location)
        }
        ScanError::MissingSftpModificationTime(location) => (
            text!("Сервер SFTP не сообщил время изменения файла"),
            location,
        ),
        ScanError::MissingSftpFileVersion(location) => (
            text!("Сервер SFTP не сообщил размер и время изменения файла"),
            location,
        ),
        ScanError::MissingSftpFileType(location) => {
            (text!("Сервер SFTP не сообщил тип объекта"), location)
        }
        ScanError::SftpCopySymlink(location) => (
            text!("Символическую ссылку нельзя скопировать по SFTP"),
            location,
        ),
        ScanError::SftpCopySpecialFile(location) => (
            text!("Специальный файл нельзя скопировать по SFTP"),
            location,
        ),
        ScanError::LocalCopySpecialFile(location) => {
            return crate::tr_format!(
                "“{}” is a special file (socket, device, or pipe). You can copy regular files, folders and symbolic links",
                location
            );
        }
        #[cfg(target_os = "macos")]
        ScanError::DiskUsageRequiresLocalFolder => {
            return text!("Анализ места доступен в локальных папках").into();
        }
        #[cfg(target_os = "macos")]
        ScanError::DiskUsageLimit => return text!("Достигнут предел анализа места").into(),
    };
    format!("{message} · {location}")
}

fn checksum_error(error: &ChecksumError) -> String {
    use ChecksumError::*;
    let (message, location) = match error {
        InvalidName(_) => return text!("Введите допустимое имя манифеста").into(),
        InvalidSelection(location) => (text!("Выберите обычные файлы из текущей папки"), location),
        UnsupportedFilename(location) => {
            return crate::tr_format!("Filename cannot be used in the manifest: {}", location);
        }
        SourceMissing(source) => {
            return crate::tr_format!("File is missing: {source}", source = source);
        }
        RequiresManifest(location) => (text!("Нужен файл манифеста SHA-256"), location),
        ManifestMissing(location) => (text!("Файл манифеста отсутствует"), location),
        InvalidLine {
            manifest,
            row,
            source,
        } => {
            let line = crate::tr_format!("Invalid SHA-256 manifest line {row}", row = row);
            let reason = match source {
                ManifestLineError::Format => text!("Ожидались контрольная сумма и имя файла"),
                ManifestLineError::Digest => text!("Ожидались 64 шестнадцатеричные цифры"),
                ManifestLineError::Escape => text!("Недопустимое экранирование имени файла"),
                ManifestLineError::Path => text!("Недопустимый относительный путь к файлу"),
            };
            return format!("{line} · {manifest} · {reason}");
        }
        DuplicatePath {
            manifest,
            row,
            path,
        } => {
            let line = crate::tr_format!("Duplicate path in manifest line {row}", row = row);
            return format!("{line} · {manifest} · {path:?}");
        }
        EmptyManifest(location) => (text!("Манифест SHA-256 пуст"), location),
        TooLarge(location) => (
            text!("Манифест SHA-256 превышает 16 МиБ или 50 000 записей"),
            location,
        ),
        DirectoryChanged(location) => (text!("Папка манифеста изменилась"), location),
    };
    format!("{message} · {location}")
}

fn permission_error(error: &PermissionError, unconfirmed: bool) -> String {
    use PermissionError::*;
    let (message, location) = match error {
        InvalidRule => return text!(
            "Введите восьмеричные или символьные права, например 0644 или u+rw,go-w.",
        ).into(),
        InvalidSelection => return text!("Выберите файлы или папки в одной папке SFTP.").into(),
        FolderChanged(location) => (
            text!("Папка SFTP изменилась. Обновите предпросмотр прав."),
            location,
        ),
        ItemChanged(location) => (
            text!("Объект SFTP изменился. Обновите предпросмотр прав."),
            location,
        ),
        MissingMode(location) => (text!("Сервер SFTP не сообщил права доступа."), location),
        UnsupportedType(location) => (
            text!("Изменение прав доступно для обычных файлов и папок."),
            location,
        ),
        SetMode { location, source } => {
            let message = if unconfirmed {
                text!("Связь с SFTP прервалась. Изменение прав не подтверждено.")
            } else if source.code() == ssh2::ErrorCode::SFTP(libssh2_sys::LIBSSH2_FX_PERMISSION_DENIED) {
                text!("Сервер SFTP отказал в доступе.")
            } else {
                text!("Не удалось изменить права доступа.")
            };
            return format!("{message} · {location}: {source}");
        }
        ModeMismatch { location, expected, actual } => {
            let message = crate::tr_format!("The server set {:04o} instead of {:04o}.", actual, expected);
            return format!("{message} · {location}");
        }
        VerifyMode(cause) if unconfirmed => return crate::tr_format!(
            "The SFTP server accepted the permission change. The resulting permissions could not be verified. {}",
            file_error(cause)
        ),
        VerifyMode(cause) => {
            return crate::tr_format!("Could not verify the new permissions. {}", file_error(cause));
        }
    };
    format!("{message} · {location}")
}

fn preview_error(error: &PreviewError) -> String {
    let (message, location) = match error {
        PreviewError::UnsupportedFile(location) => {
            let message = if location.remote() {
                text!("Предпросмотр ссылок и специальных файлов SFTP недоступен")
            } else {
                text!("Предпросмотр специальных файлов недоступен.")
            };
            (message, location)
        }
        PreviewError::ChangedBeforeRead(location) => {
            (text!("Файл изменился перед чтением"), location)
        }
        PreviewError::ChangedWhileReading(location) => (
            text!("Файл изменился во время чтения. Откройте предпросмотр заново."),
            location,
        ),
        #[cfg(target_os = "macos")]
        PreviewError::MediaLimit { location, limit } => {
            let mib = 1024 * 1024;
            let size = if limit.is_multiple_of(mib) {
                format!("{} {}", limit / mib, text!("МиБ"))
            } else {
                format!("{} {}", limit, text!("Б"))
            };
            return crate::tr_format!(
                "The SFTP media file exceeds {size}. Open a local copy of {location}.",
                size = size,
                location = location
            );
        }
        PreviewError::WorkerUnavailable => return text!("Служба предпросмотра недоступна").into(),
    };
    format!("{message} · {location}")
}

pub(crate) fn deletion_failure(failure: &DeletionFailure) -> String {
    let DeletionFailure {
        error,
        completed,
        partial_remote,
    } = failure;
    if let TransferError::Failed(Failure::Mutation(reason)) = error
        && let MutationError::Trash { path, source } = reason.as_ref()
    {
        return crate::tr_format!(
            "Could not move item “{}” to Trash: {error}. Already moved: {completed}.",
            path.display(),
            error = source,
            completed = completed
        );
    }
    let message = file_error(error);
    if *partial_remote {
        return crate::tr_format!(
            "{e}. Items fully deleted: {completed}. The current folder may have been partly deleted.",
            e = message,
            completed = completed
        );
    }
    if *completed > 0 {
        return crate::tr_format!(
            "{error}. Items processed: {n}.",
            error = message,
            n = completed
        );
    }
    message
}

pub(crate) fn transfer_failure(failure: &TransferFailure) -> String {
    let TransferFailure { error, summary } = failure;
    let mut message = file_error(error);
    if let Some(warning) = &summary.move_history_warning {
        message.push('\n');
        message.push_str(&file_error(warning));
    }
    let (completed, skipped, remaining) =
        (summary.completed, summary.skipped, summary.remaining.len());
    #[cfg(target_os = "macos")]
    if remaining == 0
        && let TransferError::Failed(Failure::Publication(reason)) = error
        && matches!(
            reason.as_ref(),
            PublicationError::RecoveryFinalization { .. }
        )
    {
        return message;
    }
    let reason = message.strip_suffix('.').unwrap_or(&message);
    if matches!(error, TransferError::PublicationUnconfirmed(_)) {
        return crate::tr_format!(
            "{error}. Server publication is unconfirmed. Completed: {completed}. Skipped: {skipped}. Remaining: {remaining}. Inspect the source and destination before retrying.", completed = completed, error = reason, remaining = remaining, skipped = skipped
        );
    }
    if summary.preparing {
        return message;
    }
    let paths = summary
        .remaining
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    if let TransferError::Failed(Failure::MoveHistory(history)) = error
        && matches!(history.as_ref(), MoveHistoryError::Descendants { .. })
    {
        return crate::tr_format!(
            "Completed: {completed}. Skipped: {skipped}. Pending: {remaining}.\n{paths}\n{error}",
            completed = completed,
            skipped = skipped,
            remaining = remaining,
            paths = paths,
            error = message
        );
    }
    crate::tr_format!(
        "{error}. Completed: {completed}. Skipped: {skipped}. Remaining: {remaining}: {paths}. Refresh both folders before retrying; check any recovery instructions.", completed = completed, error = reason, paths = paths, remaining = remaining, skipped = skipped
    )
}

fn path_error(error: &PathError) -> String {
    let message = match error {
        PathError::ArchiveAddress | PathError::ArchiveFile(_) => text!("Недопустимый адрес архива"),
        PathError::ArchiveMember {
            source: ArchiveMemberError::InvalidPath,
            ..
        } => {
            text!("Недопустимый путь внутри архива")
        }
        PathError::ArchiveMember {
            source: ArchiveMemberError::LimitExceeded,
            ..
        } => {
            text!("Слишком много данных для просмотра архива")
        }
        PathError::SftpUrl(e) => return crate::tr_format!("Invalid SFTP address: {e}", e = e),
        PathError::SftpUrlFields => {
            text!("Используйте sftp://пользователь@сервер:22/путь без пароля, query и fragment",)
        }
        PathError::MissingSftpHost => text!("Укажите сервер SFTP"),
        PathError::SftpUserEncoding(_) => text!("Имя пользователя должно быть UTF-8"),
        PathError::MissingSftpUser => {
            text!("Укажите пользователя: sftp://пользователь@сервер/путь")
        }
        PathError::SftpPathEncoding(_) => text!("Путь должен быть UTF-8"),
        PathError::SftpPathContainsNul(_) => text!("Недопустимый путь"),
        PathError::SftpHomeShorthand => {
            text!("Для домашней папки сервера используйте кнопку SFTP и начальный путь «.»")
        }
        PathError::RemotePathEncoding(_) => text!("Путь SFTP должен быть UTF-8"),
        PathError::RemotePathSyntax(_) => {
            text!("Используйте абсолютный POSIX-путь SFTP без обратных слешей")
        }
        PathError::InvalidName(_) => text!("Введите имя без /, \\, NUL, . и .."),
        PathError::InvalidLocalName(_) => text!("Введите имя файла или папки без / и \\"),
    };
    message.into()
}

fn mutation_error(error: &MutationError, unconfirmed: bool) -> String {
    use MutationError::*;
    match error {
        LocalIo {
            path,
            action,
            source,
        } => {
            let action = match action {
                LocalPathAction::OpenDirectory => text!("Не удалось открыть папку"),
                LocalPathAction::ResolveParent => text!("Не удалось определить путь к папке"),
                LocalPathAction::ResolveEntry => text!("Не удалось определить путь к элементу"),
                LocalPathAction::ReadMetadata => text!("Не удалось получить сведения об элементе"),
                LocalPathAction::CheckDestination => text!("Не удалось проверить место назначения"),
                LocalPathAction::CreateDirectory => text!("Не удалось создать папку"),
                LocalPathAction::RemoveFile => text!("Не удалось удалить исходный файл"),
                LocalPathAction::RemoveDirectory => text!("Не удалось удалить исходную папку"),
            };
            format!("{action} «{}»: {source}", path.display())
        }
        CurrentDirectory { source, .. } => {
            crate::tr_format!("Could not determine the current folder: {e}", e = source)
        }
        NotDirectory(path) => crate::tr_format!("“{}” is not a folder", path.display()),
        ModifyRoot(_) => text!("Нельзя изменять корень файловой системы").into(),
        RenameRoot(path) if path.remote() => text!("Нельзя переименовать корень").into(),
        RenameRoot(_) => text!("Нельзя переименовать корень файловой системы").into(),
        UndoUnavailable(_) => crate::tr_format!(
            "Undo unavailable: a file or folder changed or is inaccessible. Inspect both paths."
        ),
        DestinationExists(path) => crate::tr_format!(
            "“{}” already exists. Choose another name or folder. Existing items are preserved",
            path.display()
        ),
        #[cfg(target_os = "macos")]
        CompatibleNameOccupied(target) => crate::tr_format!(
            "Destination name became occupied after preview: {target}. Rebuild the plan.",
            target = target
        ),
        SourceChanged(path) => super::source_changed_during_copy(path),
        MoveIncomplete { target, cause } => crate::tr_format!(
            "A copy was created at {target}. The move is incomplete. Check the source and result history. {error}",
            target = target,
            error = file_error(cause)
        ),
        #[cfg(target_os = "macos")]
        RetainedMoveSource { held, cause } => crate::tr_format!(
            "{error} Check the original folder and retained location {held}.",
            error = file_error(cause), held = held
        ),
        #[cfg(target_os = "macos")]
        PlanChanged(location) => crate::tr_format!(
            "An item changed after planning: {location}. Build the plan again.", location = location
        ),
        #[cfg(target_os = "macos")]
        SmbSourceNotRegular(path) => format!(
            "{} · {path}",
            text!("В SMB можно скопировать обычные файлы со свободными именами.")
        ),
        ReadOnlyArchive(_) => text!("Эта операция недоступна внутри архива").into(),
        RenameLocal { from, to, source } => {
            #[cfg(unix)]
            let cross_volume = source.raw_os_error() == Some(libc::EXDEV);
            #[cfg(windows)]
            let cross_volume = source.raw_os_error() == Some(17); // ERROR_NOT_SAME_DEVICE
            #[cfg(not(any(unix, windows)))]
            let cross_volume = false;
            if cross_volume {
                return crate::tr_format!(
                    "Cross-volume move is unavailable: {error}. Copy the item, check the copy, and move the original to Trash",
                    error = source
                );
            }
            crate::tr_format!(
                "Could not move “{}” to “{}” without replacing existing items: {error}",
                from.display(),
                to.display(),
                error = source
            )
        }
        OpenSftpRename { from, to, reason } => crate::tr_format!(
            "Could not open SFTP to rename {from} to {to}: {error}",
            from = from,
            to = to,
            error = file_error(reason)
        ),
        RenameSftp { from, to, source } if unconfirmed => crate::tr_format!(
            "The server did not confirm the rename from {from} to {to}: {error}",
            from = from,
            to = to,
            error = source
        ),
        RenameSftp { from, to, source } => crate::tr_format!(
            "Could not rename SFTP item {from} to {to}: {error}",
            from = from,
            to = to,
            error = source
        ),
        CreateSftp {
            destination,
            source,
        } => format!(
            "{} «{}»: {source}",
            text!("Не удалось создать папку"),
            destination.path().display(),
        ),
        Trash { path, source } => crate::tr_format!(
            "Could not move item “{}” to Trash: {error}",
            path.display(),
            error = source
        ),
        InvalidSftpRemoval(location) => format!(
            "{} «{}»",
            text!("Недопустимый путь удаления SFTP"),
            location.path().display()
        ),
        SftpRemovalDepth(location) => format!(
            "{} «{}»",
            text!("Слишком большая глубина папок"),
            location.path().display()
        ),
        OpenSftpRemoval { location, reason } => crate::tr_format!(
            "Could not open SFTP for deletion at {location}: {error}",
            error = file_error(reason),
            location = location
        ),
        RemoveSftp { location, source, .. } if unconfirmed => crate::tr_format!(
            "SFTP deletion at {location} is unconfirmed: {error}. Check the folder before trying again",
            error = source,
            location = location
        ),
        RemoveSftp { location, source, .. } => crate::tr_format!(
            "Could not delete SFTP item {location}: {error}",
            error = source,
            location = location
        ),
    }
}

fn publication_error(error: &PublicationError) -> String {
    match error {
        PublicationError::DestinationChanged(target) => crate::tr_format!(
            "Destination {target} changed after the conflict dialog was shown",
            target = target
        ),
        PublicationError::PayloadNotRegular(payload) => crate::tr_format!(
            "Source item {payload} is no longer a regular file",
            payload = payload
        ),
        PublicationError::StagingCreation { staging, create, inspect } => {
            let message = crate::tr_format!(
                "Temporary folder creation at {staging} is unconfirmed: {e}. Inspect this location before retrying.",
                e = create, staging = staging
            );
            if let Some(inspect) = inspect {
                let verification = crate::tr_format!(
                    "Could not check whether the temporary folder exists: {error}", error = inspect
                );
                format!("{message}\n{verification}")
            } else {
                message
            }
        }
        #[cfg(target_os = "macos")]
        PublicationError::SmbUpload { target, cause } => crate::tr_format!(
            "SMB copy is unconfirmed. Check {target} for an incomplete copy before retrying ({error})",
            target = target, error = file_error(cause)
        ),
        #[cfg(target_os = "macos")]
        PublicationError::SmbTargetChanged(target) => crate::tr_format!(
            "SMB destination {target} changed during copying", target = target
        ),
        #[cfg(target_os = "macos")]
        PublicationError::SmbTargetSize { target, expected, actual } => crate::tr_format!(
            "SMB destination {target} has {actual} bytes, expected {expected}",
            target = target, actual = actual, expected = expected
        ),
        #[cfg(target_os = "macos")]
        PublicationError::RecoveryFinalization { target, moving, cause } => {
            let reason = recovery_record_error(cause);
            if *moving {
                crate::tr_format!("Move completed at {target}. {error}", target = target, error = reason)
            } else {
                crate::tr_format!("Copy completed at {target}. {error}", target = target, error = reason)
            }
        }
        #[cfg(target_os = "macos")]
        PublicationError::CompatibleOutputChanged(target) => crate::tr_format!(
            "Destination differs from the verified copy: {target}",
            target = target
        ),
        #[cfg(target_os = "macos")]
        PublicationError::CompatibleCopyUnconfirmed { target, cause } => crate::tr_format!(
            "Inspect the possibly incomplete copy at {target}. The source was preserved. {error}",
            target = target,
            error = file_error(cause)
        ),
        #[cfg(target_os = "macos")]
        PublicationError::DirectoryUnverified { target, cause } => crate::tr_format!(
            "The new folder {location} could not be verified: {error}",
            location = target,
            error = file_error(cause)
        ),
        PublicationError::Backup {
            target,
            backup,
            cause,
        } => super::initial_backup_rename_failed(
            &file_error(cause),
            target,
            backup,
            matches!(cause, TransferError::PublicationUnconfirmed(_)),
        ),
        PublicationError::Rollback {
            target,
            backup,
            cause,
            restore,
        } => {
            let unconfirmed = if cfg!(target_os = "macos")
                && matches!(cause, TransferError::Cancelled)
            {
                matches!(restore, TransferError::PublicationUnconfirmed(_))
            } else {
                target.remote()
            };
            super::replacement_rollback_failed(
                &file_error(cause),
                &file_error(restore),
                target,
                backup,
                unconfirmed,
            )
        }
        PublicationError::Retained {
            staging,
            notice,
            cause,
        } => {
            let message = file_error(cause);
            match notice {
                RecoveryNotice::InspectStaging => crate::tr_format!(
                    "{error}. Temporary folder retained for inspection at {staging}.",
                    error = message,
                    staging = staging
                ),
                #[cfg(target_os = "macos")]
                RecoveryNotice::ResumeCopy => crate::tr_format!(
                    "{message} Partial copy retained at {stage}. See Crash recovery.",
                    message = message,
                    stage = staging
                ),
                #[cfg(target_os = "macos")]
                RecoveryNotice::ExternalEditor => crate::tr_format!(
                    "{error}. Inspect the destination and retained folder {staging}.",
                    error = message,
                    staging = staging
                ),
            }
        }
    }
}

fn staging_error(error: &StagingError) -> String {
    match error {
        StagingError::MissingParent(target) => {
            format!("{} · {target}", text!("Недопустимое назначение"))
        }
        StagingError::NamesExhausted(parent) => format!(
            "{} · {parent}",
            text!("Не удалось создать временную папку для копирования; повторите попытку")
        ),
        StagingError::IdentityChanged(staging) => crate::tr_format!(
            "Staging directory identity changed at {staging}. Its contents were preserved.",
            staging = staging
        ),
        #[cfg(windows)]
        StagingError::IdentityUnavailable(staging) => crate::tr_format!(
            "Could not verify the identity of temporary folder {staging}",
            staging = staging
        ),
        StagingError::Journal {
            staging,
            action,
            source,
        } => match action {
            StagingRecordAction::Plan => crate::tr_format!(
                "Could not record temporary copy plan “{}”: {error}",
                staging,
                error = source
            ),
            StagingRecordAction::Abandon => crate::tr_format!(
                "Could not discard temporary copy plan “{}”: {error}",
                staging,
                error = source
            ),
            StagingRecordAction::Register => crate::tr_format!(
                "Could not register temporary copy “{}”: {error}",
                staging,
                error = source
            ),
        },
        #[cfg(target_os = "macos")]
        StagingError::ResumeReceipt { staging, source } => crate::tr_format!(
            "Could not save the resume record for {staging}. {error}",
            staging = staging,
            error = recovery_record_error(source)
        ),
        #[cfg(target_os = "macos")]
        StagingError::ResumeReceiptCleanup { cause, source } => {
            format!("{}\n{}", file_error(cause), recovery_record_error(source))
        }
    }
}

fn staging_cleanup_error(error: &StagingCleanupError) -> String {
    let outcome =
        error.operation.as_ref().map(file_error).unwrap_or_else(|| {
            crate::tr_format!("Item {target} was published", target = error.target)
        });
    let detail = file_error(&error.cleanup);
    if matches!(error.cleanup, TransferError::PublicationUnconfirmed(_)) {
        return crate::tr_format!(
            "{outcome}. Cleanup at {staging} is unconfirmed: {e}",
            outcome = outcome,
            staging = error.staging,
            e = detail
        );
    }
    #[cfg(target_os = "macos")]
    if matches!(error.operation, Some(TransferError::Cancelled)) {
        return crate::tr_format!(
            "{outcome}. Could not clean up the temporary folder at {staging}. {e}",
            outcome = outcome,
            staging = error.staging,
            e = detail
        );
    }
    crate::tr_format!(
        "{outcome}. Temporary folder remains at {staging}: {e}",
        outcome = outcome,
        staging = error.staging,
        e = detail
    )
}

fn listing_error(error: &ListingError) -> String {
    use ListingErrorKind::*;
    let path = if error.location.remote() {
        error.location.display()
    } else {
        error.location.path().display().to_string()
    };
    let (action, cause): (_, &dyn std::fmt::Display) = match &error.kind {
        OpenLocal(source) => (text!("Не удалось прочитать папку"), source),
        OpenSftp(source) => (text!("Не удалось прочитать папку"), source),
        ReadLocalEntry(source) => (text!("Не удалось прочитать элемент папки"), source),
        ReadSftp(source) => (text!("Не удалось прочитать элемент папки"), source),
        LocalMetadata(source) => (text!("Не удалось получить сведения об элементе"), source),
        ArchiveFormat => return text!("Поддерживаются архивы ZIP и TAR").into(),
        ReadArchive(source) => {
            return crate::tr_format!("Could not read archive “{}”: {error}", path, error = source);
        }
        MissingArchiveFolder => return text!("Папка отсутствует в архиве").into(),
        SftpNameEncoding(name) => {
            return format!(
                "{} · {path} · {name:?}",
                text!("Имя файла SFTP не является UTF-8")
            )
        }
        InvalidSftpName(source @ PathError::InvalidName(name)) => {
            return format!(
                "{} · {path} · {name:?}. {}",
                text!("Сервер SFTP вернул недопустимое имя"),
                path_error(source)
            );
        }
        InvalidSftpName(source) => return format!("{} · {path}", path_error(source)),
        RequiresSftpConnection => {
            return text!("Для просмотра дерева подключитесь по SFTP.").into()
        }
    };
    format!("{action} «{path}»: {cause}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        file_error::SftpSpaceFailure,
        vfs::{Endpoint, Location},
    };
    use ssh2::{Error, ErrorCode};

    #[test]
    fn space_probe_reports_unsupported_status_and_additional_cleanup_failure() {
        let directory = Location::Sftp {
            endpoint: Endpoint {
                host: "localhost".into(),
                port: 22,
                user: "fixture".into(),
                key: None,
                jump: None,
            },
            path: "/target".into(),
        };
        let probe = directory.join(".staging").join("space-probe");
        let report = SpaceError::SftpProbe(Box::new(SftpSpaceFailure {
            directory: directory.clone(),
            probe: probe.clone(),
            causes: vec![
                (
                    ProbeAction::Query,
                    Error::from_errno(ErrorCode::SFTP(libssh2_sys::LIBSSH2_FX_OP_UNSUPPORTED)),
                ),
                (
                    ProbeAction::Remove,
                    Error::from_errno(ErrorCode::SFTP(libssh2_sys::LIBSSH2_FX_PERMISSION_DENIED)),
                ),
            ],
        }));
        let message = space_error(&report);
        assert!(message.contains(&directory.display()), "{message}");
        assert!(
            message.contains("does not support fstatvfs@openssh.com")
                && message.contains("[SFTP(8)]"),
            "{message}"
        );
        assert!(
            message.contains(&probe.display()) && message.contains("[SFTP(3)]"),
            "{message}"
        );
    }
}
