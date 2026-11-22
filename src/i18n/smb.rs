use super::text;
use crate::file_error::CredentialFailure;
use crate::smb::{MountFailure, PasswordAction, PathAction, SmbError};

pub(crate) fn smb_error(error: &SmbError) -> String {
    let invalid = || text!("Введите SMB-адрес вида smb://сервер/папка без учётных данных.");
    match error {
        SmbError::InvalidAddress => invalid().into(),
        SmbError::AddressSyntax(source) => format!("{} ({source})", invalid()),
        SmbError::AddressEncoding(source) => format!("{} ({source})", invalid()),
        SmbError::InvalidUser => text!("Введите имя пользователя SMB.").into(),
        SmbError::InvalidPassword => text!("Пароль SMB содержит нулевой байт.").into(),
        SmbError::DataDirectoryUnavailable => text!("Нет папки данных для подключения SMB.").into(),
        SmbError::AlreadyMounted { address, path } => crate::tr_format!(
            "SMB share {address} is already mounted at {path}. Disconnect it before changing the password.",
            address = address, path = path.display()
        ),
        SmbError::Mount { address, path, cause } => {
            let error = match cause {
                MountFailure::Posix(source) => source.to_string(),
                MountFailure::Status(status) => crate::tr_format!("NetFS status {status}", status = status),
            };
            crate::tr_format!("Could not mount SMB share {address} at {path} ({error})",
                address = address, path = path.display(), error = error)
        }
        SmbError::MissingMount(path) => crate::tr_format!(
            "macOS did not find the expected SMB mount at {path}.", path = path.display()
        ),
        SmbError::MountChanged(path) => crate::tr_format!(
            "The SMB mount at {path} changed. The current mount was left in place.", path = path.display()
        ),
        SmbError::Local { action, path, source } => {
            let action = match action {
                PathAction::CreateStorage => text!("Не удалось создать папку данных SMB"),
                PathAction::ResolveStorage => text!("Не удалось определить путь к папке данных SMB"),
                PathAction::AllocateMount => text!("Не удалось выделить папку подключения SMB"),
                PathAction::PrepareMount => text!("Не удалось подготовить точку монтирования SMB"),
                PathAction::OpenMount => text!("Не удалось открыть локальную точку монтирования SMB"),
                PathAction::InspectMount => text!("Не удалось проверить точку монтирования SMB"),
                PathAction::ProtectMount => text!("Не удалось защитить локальную папку под подключением SMB"),
                PathAction::ReadShare => text!("Не удалось прочитать содержимое папки SMB"),
                PathAction::WriteRecord => text!("Не удалось записать сведения о подключении SMB"),
                PathAction::Unmount => text!("Не удалось отключить папку SMB"),
                PathAction::Cleanup => text!("Не удалось удалить локальный объект подключения SMB"),
            };
            crate::tr_format!("{action} at {path} ({error})", action = action, path = path.display(), error = source)
        }
        SmbError::EncodeRecord { path, source } => crate::tr_format!(
            "{action} at {path} ({error})",
            action = text!("Не удалось подготовить сведения о подключении SMB"), path = path.display(), error = source
        ),
        SmbError::Password { action, address, cause } => {
            let label = match action {
                PasswordAction::Read => text!("Не удалось прочитать пароль SMB из Связки ключей"),
                PasswordAction::Save => text!("Не удалось сохранить пароль SMB в Связке ключей"),
            };
            let detail = password_cause(cause);
            let message = format!("{label} · {address}\n{detail}");
            match action {
                PasswordAction::Read => message,
                PasswordAction::Save => format!("{}\n{message}", text!("Папка SMB подключена.")),
            }
        }
        SmbError::PasswordEncoding { address, source } => crate::tr_format!(
            "The saved SMB password for {address} has invalid encoding ({error}).", address = address, error = source
        ),
        SmbError::SetupCleanup { primary, cleanup } => {
            format!("{}\n{}", smb_error(primary), smb_error(cleanup))
        }
        SmbError::DisconnectCleanup { address, cleanup } => crate::tr_format!(
            "SMB share {address} disconnected. {error}", address = address, error = smb_error(cleanup)
        ),
        SmbError::WorkerDisconnected => text!("Задача SMB завершилась без результата.").into(),
    }
}

fn password_cause(cause: &CredentialFailure) -> String {
    match cause {
        CredentialFailure::Keychain(source) => crate::tr_format!(
            "Keychain error {code}: {error}",
            code = source.code(),
            error = source
        ),
        CredentialFailure::Profile(source) => source.to_string(),
        CredentialFailure::StoreUnavailable => {
            text!("Системное хранилище секретов недоступно").into()
        }
        CredentialFailure::MissingEntry => text!("Запись пароля SMB не найдена").into(),
        CredentialFailure::AmbiguousEntry => text!("Найдено несколько записей пароля SMB").into(),
        CredentialFailure::Prepare(_)
        | CredentialFailure::Malformed(_)
        | CredentialFailure::InvalidEntry => text!("Запись пароля SMB недействительна").into(),
        CredentialFailure::AttributeTooLong {
            secret: true,
            limit,
        } => crate::tr_format!(
            "The saved password exceeds the credential store limit ({limit} bytes).",
            limit = limit
        ),
        CredentialFailure::AttributeTooLong {
            secret: false,
            limit,
        } => crate::tr_format!(
            "A credential attribute exceeds the store limit ({limit} bytes).",
            limit = limit
        ),
    }
}
