use super::text;
use crate::file_error::{
    ArchiveMemberError, Failure, ListingError, ListingErrorKind, LocalPathAction, MutationError,
    PathError,
};
use crate::operations::TransferError;
use crate::vfs::TransferFailure;

pub(crate) fn file_error(error: &TransferError) -> String {
    match error {
        TransferError::Failed(Failure::Path(error)) => path_error(error),
        TransferError::Failed(Failure::Listing(error)) => listing_error(error),
        TransferError::Failed(Failure::ConflictClosed) => text!("Диалог конфликта закрыт").into(),
        TransferError::Failed(Failure::Mutation(error)) => mutation_error(error),
        _ => error.to_string(),
    }
}

pub(crate) fn transfer_failure(failure: &TransferFailure) -> String {
    let TransferFailure { error, summary } = failure;
    let message = file_error(error);
    let (completed, skipped, remaining) =
        (summary.completed, summary.skipped, summary.remaining.len());
    if matches!(error, TransferError::PublicationUnconfirmed(_)) {
        return crate::tr_format!(
            "{error}. Server publication is unconfirmed. Completed: {completed}. Skipped: {skipped}. Remaining: {remaining}. Inspect the source and destination before retrying.", completed = completed, error = message, remaining = remaining, skipped = skipped
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
    crate::tr_format!(
        "{error}. Completed: {completed}. Skipped: {skipped}. Remaining: {remaining}: {paths}. Refresh both folders before retrying; check any recovery instructions.", completed = completed, error = message, paths = paths, remaining = remaining, skipped = skipped
    )
}

pub(crate) fn path_error(error: &PathError) -> String {
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

fn mutation_error(error: &MutationError) -> String {
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
        CreateSftp {
            destination,
            source,
        } => format!(
            "{} «{}»: {source}",
            text!("Не удалось создать папку"),
            destination.path().display(),
        ),
    }
}

fn listing_error(error: &ListingError) -> String {
    use ListingErrorKind::*;
    let path = error.location.path().display();
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
        SftpNameEncoding(_) => return text!("Имя файла SFTP не является UTF-8").into(),
        InvalidSftpName(source) => return path_error(source),
        RequiresSftpConnection => {
            return text!("Для просмотра дерева подключитесь по SFTP.").into()
        }
    };
    format!("{action} «{path}»: {cause}")
}
