//! Interface translations. Filesystem paths and user content never pass through this catalog.
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU8, Ordering};

mod catalog;
pub(crate) mod display;
mod file_errors;
mod formats;
pub(crate) use catalog::key;
pub(crate) use file_errors::{
    access_io_message, connection_result_error, deletion_failure, file_error, history_error,
    metadata_warning, recovery_record_error, replacement_refusal, transfer_failure,
};
#[cfg(target_os = "macos")]
pub(crate) use formats::czech_format;
pub(crate) use formats::russian_format;

#[cfg(target_os = "macos")]
pub(crate) fn resume_completion(report: crate::vfs::resume_copy::ResumeCompletion) -> String {
    use crate::vfs::resume_copy::Completion;

    let mut message = match (report.moving, report.kind) {
        (true, Completion::Copied(offset)) => crate::tr_format!(
            "Move resumed at byte {offset} and completed.",
            offset = offset
        ),
        (true, Completion::FolderCopied(bytes)) => crate::tr_format!(
            "Folder move completed. Reused {bytes} staged bytes.",
            bytes = bytes
        ),
        (true, Completion::AlreadyPresent) => text!("Перенос завершён после сверки копии.").into(),
        (false, Completion::Copied(offset)) => crate::tr_format!(
            "Copy resumed at byte {offset} and completed.",
            offset = offset
        ),
        (false, Completion::FolderCopied(bytes)) => crate::tr_format!(
            "Folder copy completed. Reused {bytes} staged bytes.",
            bytes = bytes
        ),
        (false, Completion::AlreadyPresent) => {
            text!("В назначении подтверждена полная копия.").into()
        }
    };
    if report.metadata_warning {
        message.push('\n');
        message.push_str(text!(
            "Даты или права доступа сохранены частично. Подробности указаны в строках журнала."
        ));
    }
    message
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Language {
    Russian,
    #[default]
    English,
    #[cfg(target_os = "macos")]
    Czech,
}

static LANGUAGE: AtomicU8 = AtomicU8::new(Language::English as u8);

pub fn set_language(language: Language) {
    LANGUAGE.store(language as u8, Ordering::Relaxed);
}

pub(crate) fn language() -> Language {
    match LANGUAGE.load(Ordering::Relaxed) {
        value if value == Language::Russian as u8 => Language::Russian,
        #[cfg(target_os = "macos")]
        value if value == Language::Czech as u8 => Language::Czech,
        _ => Language::English,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct MessageKey {
    russian: &'static str,
    english: &'static str,
    #[cfg(target_os = "macos")]
    czech: &'static str,
}

impl MessageKey {
    #[doc(hidden)]
    pub(crate) const fn from_catalog(
        russian: &'static str,
        english: &'static str,
        czech: &'static str,
    ) -> Self {
        #[cfg(not(target_os = "macos"))]
        let _ = czech;
        Self {
            russian,
            english,
            #[cfg(target_os = "macos")]
            czech,
        }
    }

    fn translated(self, language: Language) -> &'static str {
        match language {
            Language::English => self.english,
            Language::Russian => self.russian,
            #[cfg(target_os = "macos")]
            Language::Czech => self.czech,
        }
    }

    pub(crate) fn matches(self, value: &str) -> bool {
        let matches = self.russian == value || self.english == value;
        #[cfg(target_os = "macos")]
        let matches = matches || self.czech == value;
        matches
    }
}

pub(crate) fn text(key: MessageKey) -> &'static str {
    key.translated(language())
}

macro_rules! translate_literal {
    ($message:tt $(,)?) => {
        $crate::i18n::text($crate::i18n::key!($message))
    };
}
pub(crate) use translate_literal as text;

#[cfg(target_os = "macos")]
pub(crate) fn cli_help() -> &'static str {
    match language() {
        Language::English => include_str!("i18n/help/en.txt"),
        Language::Russian => include_str!("i18n/help/ru.txt"),
        Language::Czech => include_str!("i18n/help/cs.txt"),
    }
    .trim_end_matches('\n')
}

pub(crate) fn transfer_finished(
    completed: usize,
    skipped: usize,
    remaining: &[String],
    preparing: bool,
    cancelled: bool,
    destination: &str,
) -> String {
    format_transfer_finished(
        language(),
        (completed, skipped),
        remaining,
        preparing,
        cancelled,
        destination,
    )
}

fn format_transfer_finished(
    language: Language,
    counts: (usize, usize),
    remaining: &[String],
    preparing: bool,
    cancelled: bool,
    destination: &str,
) -> String {
    let (completed, skipped) = counts;
    let mut message = match (language, preparing, cancelled) {
        #[cfg(target_os = "macos")]
        (Language::Czech, true, _) => {
            "Příprava byla zrušena. Vybrané cesty zůstaly beze změny.".into()
        }
        #[cfg(target_os = "macos")]
        (Language::Czech, false, true) => format!(
            "Zastaveno. Hotovo: {completed}. Přeskočeno: {skipped}. Zbývá: {}.",
            remaining.len()
        ),
        #[cfg(target_os = "macos")]
        (Language::Czech, false, false) => {
            format!("Hotovo: {completed}. Přeskočeno: {skipped}. {destination}")
        }
        (Language::English, true, _) => {
            "Cancelled while preparing; selected paths left unchanged.".to_owned()
        }
        (Language::Russian, true, _) => {
            "Отменено при подготовке; выбранные пути не изменены.".to_owned()
        }
        (Language::English, false, true) => format!(
            "Stopped. Completed: {completed}. Skipped: {skipped}. Remaining: {}.",
            remaining.len()
        ),
        (Language::Russian, false, true) => format!(
            "Остановлено. Готово: {completed}. Пропущено: {skipped}. Осталось: {}.",
            remaining.len()
        ),
        (Language::English, false, false) => {
            format!("Completed: {completed}. Skipped: {skipped}. {destination}")
        }
        (Language::Russian, false, false) => {
            format!("Готово: {completed}. Пропущено: {skipped}. {destination}")
        }
    };
    if !remaining.is_empty() {
        message.push_str(match (language, preparing) {
            #[cfg(target_os = "macos")]
            (Language::Czech, true) => "\nVybrané cesty:",
            #[cfg(target_os = "macos")]
            (Language::Czech, false) => "\nZbývající cesty:",
            (Language::English, true) => "\nSelected paths:",
            (Language::Russian, true) => "\nВыбранные пути:",
            (Language::English, false) => "\nRemaining paths:",
            (Language::Russian, false) => "\nОставшиеся пути:",
        });
        for path in remaining {
            message.push('\n');
            message.push_str(path);
        }
    }
    message
}

pub(crate) fn source_changed_during_copy(source: impl std::fmt::Display) -> String {
    format_source_changed(language(), source)
}

fn format_source_changed(language: Language, source: impl std::fmt::Display) -> String {
    match language {
        #[cfg(target_os = "macos")]
        Language::Czech => format!("Zdroj „{source}“ se během kopírování změnil"),
        Language::English => format!("Source “{source}” changed during copying"),
        Language::Russian => format!("Источник «{source}» изменился во время копирования"),
    }
}

pub(crate) fn initial_backup_rename_failed(
    error: &str,
    target: impl std::fmt::Display,
    backup: impl std::fmt::Display,
    remote: bool,
) -> String {
    format_initial_backup_rename_failed(language(), error, target, backup, remote)
}

fn format_initial_backup_rename_failed(
    language: Language,
    error: &str,
    target: impl std::fmt::Display,
    backup: impl std::fmt::Display,
    remote: bool,
) -> String {
    match (language, remote) {
        #[cfg(target_os = "macos")]
        (Language::Czech, false) => format!("{error}. Původní cíl zůstal na {target}"),
        #[cfg(target_os = "macos")]
        (Language::Czech, true) => format!("{error}. Před opakováním zkontrolujte původní cíl {target} a možnou zálohu {backup}"),
        (Language::English, false) => format!("{error}. The existing destination remains at {target}"),
        (Language::Russian, false) => {
            format!("{error}. Прежний файл сохранён по исходному пути {target}")
        }
        (Language::English, true) => format!(
            "{error}. Check the original destination {target} and the possible backup {backup} before retrying"
        ),
        (Language::Russian, true) => format!(
            "{error}. Перед повтором проверьте исходное назначение {target} и возможную резервную копию {backup}"
        ),
    }
}

pub(crate) fn replacement_rollback_failed(
    error: &str,
    rollback: &str,
    target: impl std::fmt::Display,
    backup: impl std::fmt::Display,
    unconfirmed: bool,
) -> String {
    format_replacement_rollback_failed(language(), error, rollback, target, backup, unconfirmed)
}

fn format_replacement_rollback_failed(
    language: Language,
    error: &str,
    rollback: &str,
    target: impl std::fmt::Display,
    backup: impl std::fmt::Display,
    unconfirmed: bool,
) -> String {
    match (language, unconfirmed) {
        #[cfg(target_os = "macos")]
        (Language::Czech, false) => format!("{error}. Předchozí položku se nepodařilo obnovit: {rollback}. Před opakováním zkontrolujte uloženou verzi v {backup}"),
        #[cfg(target_os = "macos")]
        (Language::Czech, true) => format!("{error}. Obnovení předchozí položky se nepodařilo potvrdit: {rollback}. Před opakováním zkontrolujte původní cíl {target} a možnou zálohu {backup}"),
        (Language::English, false) => format!(
            "{error}. Could not restore the previous item: {rollback}. Check the saved version at {backup} before retrying"
        ),
        (Language::Russian, false) => format!(
            "{error}. Не удалось восстановить прежний объект: {rollback}. Перед повтором проверьте сохранённую версию в {backup}"
        ),
        (Language::English, true) => format!(
            "{error}. Could not confirm restoration of the previous item: {rollback}. Check the original destination {target} and the possible backup {backup} before retrying"
        ),
        (Language::Russian, true) => format!(
            "{error}. Не удалось подтвердить восстановление прежнего объекта: {rollback}. Перед повтором проверьте исходное назначение {target} и возможную резервную копию {backup}"
        ),
    }
}

// Pass named values explicitly so Rust checks the arguments of every template.
macro_rules! tr_format {
    ($message:tt $($args:tt)*) => {
        match $crate::i18n::language() {
            $crate::i18n::Language::English => format!($message $($args)*),
            $crate::i18n::Language::Russian => {
                format!($crate::i18n::russian_format!($message) $($args)*)
            }
            #[cfg(target_os = "macos")]
            $crate::i18n::Language::Czech => {
                format!($crate::i18n::czech_format!($message) $($args)*)
            }
        }
    };
}

pub(crate) use tr_format;

#[cfg(test)]
mod tests {
    use super::*;

    fn original_language(english: bool) -> Language {
        if english {
            Language::English
        } else {
            Language::Russian
        }
    }

    #[test]
    fn transfer_outcomes_preserve_counts_and_full_paths_in_both_languages() {
        let paths = vec![
            "/tmp/Документы {second}/file.txt".into(),
            "/tmp/third/file.txt".into(),
        ];
        for english in [true, false] {
            let stopped = format_transfer_finished(
                original_language(english),
                (1, 2),
                &paths,
                false,
                true,
                "/destination",
            );
            assert!(stopped.contains(if english {
                "Stopped. Completed: 1. Skipped: 2. Remaining: 2."
            } else {
                "Остановлено. Готово: 1. Пропущено: 2. Осталось: 2."
            }));
            for path in &paths {
                assert!(stopped.contains(path));
            }
            let preparing = format_transfer_finished(
                original_language(english),
                (0, 0),
                &paths,
                true,
                true,
                "/destination",
            );
            assert!(preparing.contains(if english {
                "selected paths left unchanged"
            } else {
                "выбранные пути не изменены"
            }));
            assert!(!preparing.contains(if english {
                "Remaining: 2"
            } else {
                "Осталось: 2"
            }));
            for path in &paths {
                assert!(preparing.contains(path));
            }
            let success = format_transfer_finished(
                original_language(english),
                (3, 2),
                &[],
                false,
                false,
                "/destination",
            );
            assert_eq!(
                success,
                if english {
                    "Completed: 3. Skipped: 2. /destination"
                } else {
                    "Готово: 3. Пропущено: 2. /destination"
                }
            );
        }
    }

    #[test]
    fn rollback_guidance_distinguishes_local_failure_and_unknown_sftp_outcome_in_both_languages() {
        let target = "/Документы/original {name}.txt";
        let backup = "/Документы/.staging/previous";
        for (english, local_guidance, remote_guidance) in [
            (
                true,
                format!("publish error. Could not restore the previous item: rollback error. Check the saved version at {backup} before retrying"),
                format!("publish error. Could not confirm restoration of the previous item: rollback error. Check the original destination {target} and the possible backup {backup} before retrying"),
            ),
            (
                false,
                format!("publish error. Не удалось восстановить прежний объект: rollback error. Перед повтором проверьте сохранённую версию в {backup}"),
                format!("publish error. Не удалось подтвердить восстановление прежнего объекта: rollback error. Перед повтором проверьте исходное назначение {target} и возможную резервную копию {backup}"),
            ),
        ] {
            let local = format_replacement_rollback_failed(
                original_language(english), "publish error", "rollback error", target, backup, false,
            );
            let remote = format_replacement_rollback_failed(
                original_language(english), "publish error", "rollback error", target, backup, true,
            );
            assert_eq!(local, local_guidance);
            assert_eq!(remote, remote_guidance);
            assert!(!local.contains(target));
            assert!(remote.contains(target) && remote.contains(backup));
        }
    }

    #[test]
    fn initial_backup_rename_guidance_distinguishes_local_and_ambiguous_sftp_in_both_languages() {
        let target = "/Документы/original {name}.txt";
        let backup = "/Документы/.staging/previous";
        for (english, local_guidance, remote_guidance) in [
            (
                true,
                format!("denied. The existing destination remains at {target}"),
                format!("denied. Check the original destination {target} and the possible backup {backup} before retrying"),
            ),
            (
                false,
                format!("denied. Прежний файл сохранён по исходному пути {target}"),
                format!("denied. Перед повтором проверьте исходное назначение {target} и возможную резервную копию {backup}"),
            ),
        ] {
            let local =
                format_initial_backup_rename_failed(original_language(english), "denied", target, backup, false);
            let remote =
                format_initial_backup_rename_failed(original_language(english), "denied", target, backup, true);
            assert_eq!(local, local_guidance);
            assert_eq!(remote, remote_guidance);
            assert!(!local.contains(backup));
            assert!(remote.contains(target) && remote.contains(backup));
        }
    }

    #[test]
    fn source_change_error_preserves_the_path_in_both_languages() {
        let source = "/tmp/Документы {name}.txt";
        assert_eq!(
            format_source_changed(Language::English, source),
            format!("Source “{source}” changed during copying")
        );
        assert_eq!(
            format_source_changed(Language::Russian, source),
            format!("Источник «{source}» изменился во время копирования")
        );
    }

    #[test]
    fn catalog_entries_have_unique_russian_keys_and_nonempty_translations() {
        let mut seen = std::collections::HashSet::new();
        for message in catalog::MESSAGES {
            assert!(!message.russian.trim().is_empty());
            assert!(!message.english.trim().is_empty(), "{}", message.russian);
            #[cfg(target_os = "macos")]
            assert!(!message.czech.trim().is_empty(), "{}", message.russian);
            assert!(seen.insert(message.russian), "{}", message.russian);
        }
    }

    #[test]
    fn checked_keys_preserve_labels_in_both_languages() {
        for (key, ru, en) in [
            (key!("Настройки"), "Настройки", "Settings"),
            (key!("Домашняя"), "Домашняя", "Home"),
            (
                key!("Удалить с сервера навсегда?"),
                "Удалить с сервера навсегда?",
                "Permanently delete from server?",
            ),
            (
                key!("Системный буфер изменился. Повторите вставку."),
                "Системный буфер изменился. Повторите вставку.",
                "System clipboard changed. Paste again.",
            ),
        ] {
            assert_eq!(key.translated(Language::English), en);
            assert_eq!(key.translated(Language::Russian), ru);
        }
    }
}
