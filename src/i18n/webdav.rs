use super::text;
use crate::webdav::WebDavError;

pub(crate) fn webdav_error(error: &WebDavError) -> String {
    match error {
        WebDavError::InvalidAddress => {
            text!("Введите HTTPS-адрес папки WebDAV без учётных данных, параметров и фрагмента.")
                .into()
        }
        WebDavError::InvalidUser => text!("Введите имя пользователя WebDAV.").into(),
        WebDavError::InvalidPassword => {
            text!("Пароль приложения WebDAV содержит недопустимые символы.").into()
        }
        WebDavError::Cancelled => text!("Чтение папки WebDAV отменено.").into(),
        WebDavError::StartFailed => text!("Не удалось запустить запрос WebDAV.").into(),
        WebDavError::Io => {
            text!("Не удалось обменяться данными с WebDAV. Повторите запрос.").into()
        }
        WebDavError::RequestFailed(Some(60)) => text!("Не удалось подтвердить сертификат HTTPS сервера WebDAV. Используйте адрес с доверенным сертификатом.").into(),
        WebDavError::RequestFailed(Some(28)) => text!("Сервер WebDAV не ответил вовремя. Проверьте сеть и повторите запрос.").into(),
        WebDavError::RequestFailed(_) => {
            text!("Не удалось подключиться к WebDAV. Проверьте адрес, сеть и сертификат HTTPS.")
                .into()
        }
        WebDavError::HttpStatus(401) => {
            text!("WebDAV отклонил учётные данные. Проверьте имя пользователя и пароль приложения.")
                .into()
        }
        WebDavError::HttpStatus(403) => text!("Сервер WebDAV запретил чтение этой папки.").into(),
        WebDavError::HttpStatus(404) => {
            text!("Папка WebDAV не найдена. Проверьте адрес из настроек сервера.").into()
        }
        WebDavError::HttpStatus(status) => {
            crate::tr_format!("WebDAV server returned HTTP {status}.", status = status)
        }
        WebDavError::ResponseTooLarge | WebDavError::TooManyEntries => {
            text!("Папка WebDAV превышает безопасный предел просмотра. Откройте меньшую папку.")
                .into()
        }
        WebDavError::InvalidResponse => {
            text!("Сервер вернул некорректный список папки WebDAV.").into()
        }
        WebDavError::UnsafeHref => {
            text!("Список WebDAV содержит адрес вне запрошенной папки. Просмотр остановлен.").into()
        }
        WebDavError::NotCollection => text!("Адрес WebDAV не указывает на доступную папку.").into(),
    }
}
