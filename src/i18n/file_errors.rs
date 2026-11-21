use super::text;
use crate::file_error::{
    AccessAction, AccessCause, AccessError, ArchiveError, ArchiveMemberError, ChecksumError,
    ConnectionError, Failure, HostKeyError, KnownHostEntryError, KnownHostsError, ListingError,
    ListingErrorKind, LocalPathAction, ManifestLineError, MutationError, PathError,
    PermissionError, PinError, PreviewError, PublicationError, RecoveryNotice, ScanError,
    SearchError, SshStage, StagingCleanupError,
};
use crate::operations::TransferError;
use crate::vfs::{DeletionFailure, TransferFailure};

pub(crate) fn file_error(error: &TransferError) -> String {
    match error {
        TransferError::Failed(reason) | TransferError::PublicationUnconfirmed(reason) => {
            match reason {
                Failure::Path(error) => path_error(error),
                Failure::Connection(error) => connection_error(error),
                Failure::Access(error) => access_error(error),
                Failure::Preview(error) => preview_error(error),
                Failure::Search(error) => search_error(error),
                Failure::Scan(error) => scan_error(error),
                Failure::Checksum(error) => checksum_error(error),
                Failure::Archive(error) => archive_error(error),
                Failure::Permissions(reason) => permission_error(
                    reason,
                    matches!(error, TransferError::PublicationUnconfirmed(_)),
                ),
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
                Failure::Mutation(reason) => mutation_error(
                    reason,
                    matches!(error, TransferError::PublicationUnconfirmed(_)),
                ),
                Failure::Publication(reason) => publication_error(reason),
                Failure::StagingCleanup(error) => staging_cleanup_error(error),
                _ => error.to_string(),
            }
        }
        TransferError::Cancelled => crate::tr_format!("Operation cancelled"),
    }
}

pub(crate) fn connection_result_error(error: &TransferError) -> String {
    if matches!(error, TransferError::Cancelled) {
        text!("Подключение отменено").into()
    } else {
        file_error(error)
    }
}

fn archive_error(error: &ArchiveError) -> String {
    match error {
        ArchiveError::InvalidName(_) => text!("Введите допустимое имя архива").into(),
        ArchiveError::SelectCurrentFolder => {
            text!("Выберите файлы или папки из текущей папки").into()
        }
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
        ArchiveError::Verify { archive, source } => format!("{archive}: {source}"),
        ArchiveError::ChangedDuringVerification(archive) => {
            format!("{} · {archive}", text!("Архив изменился во время проверки"))
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
        AccessAction::ReadMetadata | AccessAction::ReadOpenMetadata => {
            text!("Не удалось получить сведения об элементе")
        }
        AccessAction::OpenFile => {
            return crate::tr_format!("Could not open {}: {error}", error.location, error = cause);
        }
        AccessAction::ReadChildren => text!("Не удалось прочитать папку"),
        AccessAction::ReadFile => text!("Не удалось прочитать файл"),
        AccessAction::ReadLink => text!("Не удалось прочитать символическую ссылку"),
        AccessAction::WriteFile => text!("Не удалось записать файл"),
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
                "{} · {directory}/{name:?}",
                text!("Папка содержит недопустимое имя")
            );
        }
        ScanError::DepthLimit(location) => (text!("Достигнут предел вложенности папок"), location),
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
        ScanError::MissingFileVersion(location) => (
            text!("Сервер не передал размер или время изменения файла"),
            location,
        ),
        #[cfg(target_os = "macos")]
        ScanError::MissingLocalFileSize => return text!("Размер файла недоступен").into(),
        #[cfg(target_os = "macos")]
        ScanError::MissingSftpFileSize(location) => {
            (text!("Сервер SFTP не сообщил размер файла"), location)
        }
        ScanError::MissingSftpFileType(location) => {
            (text!("Сервер SFTP не сообщил тип объекта"), location)
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
        SourceChanged(path) => super::source_changed_during_copy(path),
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
        } => super::replacement_rollback_failed(
            &file_error(cause),
            &file_error(restore),
            target,
            backup,
            target.remote(),
        ),
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
                "{} · {path}/{name:?}",
                text!("Имя файла SFTP не является UTF-8")
            )
        }
        InvalidSftpName(source) => return format!("{} · {path}", path_error(source)),
        RequiresSftpConnection => {
            return text!("Для просмотра дерева подключитесь по SFTP.").into()
        }
    };
    format!("{action} «{path}»: {cause}")
}
