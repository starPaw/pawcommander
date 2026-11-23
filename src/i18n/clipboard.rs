use super::text;
use crate::file_clipboard::{
    ClipboardReadError, ClipboardReadFailure, ClipboardWriteError, LocalFileUrlError,
};

pub(crate) fn clipboard_write_error(error: &ClipboardWriteError) -> String {
    match error {
        ClipboardWriteError::PathContainsNul(path) => crate::tr_format!(
            "The path {path} contains a null byte.",
            path = format!("{path:?}")
        ),
        ClipboardWriteError::Rejected => {
            text!("AppKit отклонил запись файлов в системный буфер.").into()
        }
        ClipboardWriteError::Changed => {
            text!("Системный буфер изменился во время записи. Скопируйте выделенное ещё раз.")
                .into()
        }
    }
}

pub(crate) fn clipboard_read_error(error: &ClipboardReadError) -> String {
    let ClipboardReadError::Unreadable { cause, .. } = error else {
        return text!("Системный буфер изменился. Повторите вставку.").into();
    };
    let item_error = |slot, error: &str| {
        crate::tr_format!(
            "System clipboard item {item}. {error}",
            item = slot + 1,
            error = error
        )
    };
    match cause {
        ClipboardReadFailure::ItemsUnavailable => {
            text!("AppKit не вернул список элементов системного буфера.").into()
        }
        ClipboardReadFailure::FileUrlUnavailable(slot) => {
            item_error(*slot, text!("AppKit не вернул объявленный файловый URL."))
        }
        ClipboardReadFailure::InvalidFileUrl { slot, cause } => {
            let reason = match cause {
                LocalFileUrlError::Parse(source) => format!(
                    "{} ({source})",
                    text!("Файловый буфер содержит недопустимый URL")
                ),
                LocalFileUrlError::Unsupported => {
                    text!("Файловый буфер содержит недопустимый или нелокальный URL").into()
                }
            };
            item_error(*slot, &reason)
        }
        ClipboardReadFailure::FoundationRejectedUrl(slot) => {
            item_error(*slot, text!("Foundation отклонил файловый URL."))
        }
        ClipboardReadFailure::UnresolvedReference { slot, path } => {
            let reason = crate::tr_format!(
                "Finder could not resolve the file reference {path}.",
                path = path.display()
            );
            item_error(*slot, &reason)
        }
        ClipboardReadFailure::InvalidPath(path) => crate::tr_format!(
            "Expected a named absolute path in the file clipboard. Received {path}.",
            path = path.display()
        ),
    }
}
