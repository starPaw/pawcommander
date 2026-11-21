//! Interface translations. Filesystem paths and user content never pass through this catalog.
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};

mod catalog;
pub(crate) use catalog::key;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    Russian,
    #[default]
    English,
}

static ENGLISH: AtomicBool = AtomicBool::new(true);

pub fn set_language(language: Language) {
    ENGLISH.store(language == Language::English, Ordering::Relaxed);
}

pub fn is_english() -> bool {
    ENGLISH.load(Ordering::Relaxed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct MessageKey {
    russian: &'static str,
    english: &'static str,
}

impl MessageKey {
    #[doc(hidden)]
    pub(crate) const fn from_catalog(russian: &'static str, english: &'static str) -> Self {
        Self { russian, english }
    }

    fn translate(self, english: bool) -> &'static str {
        if english {
            self.english
        } else {
            self.russian
        }
    }
}

pub(crate) fn text(key: MessageKey) -> &'static str {
    key.translate(is_english())
}

macro_rules! translate_literal {
    ($message:tt $(,)?) => {
        $crate::i18n::text($crate::i18n::key!($message))
    };
}
pub(crate) use translate_literal as text;

#[cfg(target_os = "macos")]
pub(crate) fn cli_help() -> &'static str {
    if is_english() {
        "PawCommander\n\n\
        Usage\n  \
        pawcommander [FOLDER]\n  \
        pawcommander open FOLDER\n  \
        pawcommander workspace NAME\n  \
        pawcommander search QUERY [FOLDER]\n  \
        pawcommander --help\n\n\
        With no arguments, restore the saved session.\n\
        Open a folder, restore a named workspace, or search for file names and masks.\n\
        Search uses the current directory when FOLDER is omitted.\n\
        The open, workspace and search commands use separate windows with temporary settings.\n\
        Use -- before operands that start with a dash.\n\n\
        Exit codes\n  \
        0  Help displayed or window closed normally\n  \
        1  Settings or application startup error\n  \
        2  Invalid arguments\n  \
        3  Folder or workspace unavailable"
    } else {
        "PawCommander\n\n\
        Использование\n  \
        pawcommander [ПАПКА]\n  \
        pawcommander open ПАПКА\n  \
        pawcommander workspace ИМЯ\n  \
        pawcommander search ЗАПРОС [ПАПКА]\n  \
        pawcommander --help\n\n\
        При запуске без аргументов восстанавливается сохранённый сеанс.\n\
        Откройте папку, сохранённое рабочее пространство или поиск имён и масок.\n\
        Если папка поиска не указана, используется текущая директория.\n\
        Команды open, workspace и search открывают отдельные окна с временными настройками.\n\
        Укажите -- перед аргументами, начинающимися с дефиса.\n\n\
        Коды завершения\n  \
        0  Справка выведена или окно закрыто штатно\n  \
        1  Ошибка настроек или запуска приложения\n  \
        2  Недопустимые аргументы\n  \
        3  Папка или рабочее пространство недоступны"
    }
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
        is_english(),
        (completed, skipped),
        remaining,
        preparing,
        cancelled,
        destination,
    )
}

fn format_transfer_finished(
    english: bool,
    counts: (usize, usize),
    remaining: &[String],
    preparing: bool,
    cancelled: bool,
    destination: &str,
) -> String {
    let (completed, skipped) = counts;
    let mut message = match (english, preparing, cancelled) {
        (true, true, _) => "Cancelled while preparing; selected paths left unchanged.".to_owned(),
        (false, true, _) => "Отменено при подготовке; выбранные пути не изменены.".to_owned(),
        (true, false, true) => format!(
            "Stopped. Completed: {completed}. Skipped: {skipped}. Remaining: {}.",
            remaining.len()
        ),
        (false, false, true) => format!(
            "Остановлено. Готово: {completed}. Пропущено: {skipped}. Осталось: {}.",
            remaining.len()
        ),
        (true, false, false) => {
            format!("Completed: {completed}. Skipped: {skipped}. {destination}")
        }
        (false, false, false) => {
            format!("Готово: {completed}. Пропущено: {skipped}. {destination}")
        }
    };
    if !remaining.is_empty() {
        message.push_str(match (english, preparing) {
            (true, true) => "\nSelected paths:",
            (false, true) => "\nВыбранные пути:",
            (true, false) => "\nRemaining paths:",
            (false, false) => "\nОставшиеся пути:",
        });
        for path in remaining {
            message.push('\n');
            message.push_str(path);
        }
    }
    message
}

pub(crate) fn source_changed_during_copy(source: impl std::fmt::Display) -> String {
    format_source_changed(is_english(), source)
}

fn format_source_changed(english: bool, source: impl std::fmt::Display) -> String {
    if english {
        format!("Source “{source}” changed during copying; the copy was not published. Try again after writing to the source has finished")
    } else {
        format!("Источник «{source}» изменился во время копирования; копия не опубликована. Повторите попытку после завершения записи в источник")
    }
}

pub(crate) fn initial_backup_rename_failed(
    error: &str,
    target: impl std::fmt::Display,
    backup: impl std::fmt::Display,
    remote: bool,
) -> String {
    format_initial_backup_rename_failed(is_english(), error, target, backup, remote)
}

fn format_initial_backup_rename_failed(
    english: bool,
    error: &str,
    target: impl std::fmt::Display,
    backup: impl std::fmt::Display,
    remote: bool,
) -> String {
    match (english, remote) {
        (true, false) => format!("{error}. The existing destination remains at {target}"),
        (false, false) => {
            format!("{error}. Прежний файл сохранён по исходному пути {target}")
        }
        (true, true) => format!(
            "{error}. Check the original destination {target} and the possible backup {backup} before retrying"
        ),
        (false, true) => format!(
            "{error}. Перед повтором проверьте исходное назначение {target} и возможную резервную копию {backup}"
        ),
    }
}

pub(crate) fn replacement_rollback_failed(
    error: &str,
    rollback: &str,
    target: impl std::fmt::Display,
    backup: impl std::fmt::Display,
    remote: bool,
) -> String {
    format_replacement_rollback_failed(is_english(), error, rollback, target, backup, remote)
}

fn format_replacement_rollback_failed(
    english: bool,
    error: &str,
    rollback: &str,
    target: impl std::fmt::Display,
    backup: impl std::fmt::Display,
    remote: bool,
) -> String {
    match (english, remote) {
        (true, false) => format!(
            "{error}. Could not restore the previous file: {rollback}. Check the saved version at {backup} before retrying"
        ),
        (false, false) => format!(
            "{error}. Не удалось восстановить прежний файл: {rollback}. Перед повтором проверьте сохранённую версию в {backup}"
        ),
        (true, true) => format!(
            "{error}. Could not confirm restoration of the previous file: {rollback}. Check the original destination {target} and the possible backup {backup} before retrying"
        ),
        (false, true) => format!(
            "{error}. Не удалось подтвердить восстановление прежнего файла: {rollback}. Перед повтором проверьте исходное назначение {target} и возможную резервную копию {backup}"
        ),
    }
}

// Both format strings are supplied at the call site, preserving named captures
// and compile-time validation of the placeholders in both languages.
#[macro_export]
macro_rules! tr_format {
    ($ru:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        if $crate::i18n::is_english() {
            format!($en $(, $arg)*)
        } else {
            format!($ru $(, $arg)*)
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_outcomes_preserve_counts_and_full_paths_in_both_languages() {
        let paths = vec![
            "/tmp/Документы {second}/file.txt".into(),
            "/tmp/third/file.txt".into(),
        ];
        for english in [true, false] {
            let stopped =
                format_transfer_finished(english, (1, 2), &paths, false, true, "/destination");
            assert!(stopped.contains(if english {
                "Stopped. Completed: 1. Skipped: 2. Remaining: 2."
            } else {
                "Остановлено. Готово: 1. Пропущено: 2. Осталось: 2."
            }));
            for path in &paths {
                assert!(stopped.contains(path));
            }
            let preparing =
                format_transfer_finished(english, (0, 0), &paths, true, true, "/destination");
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
            let success =
                format_transfer_finished(english, (3, 2), &[], false, false, "/destination");
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
                format!("publish error. Could not restore the previous file: rollback error. Check the saved version at {backup} before retrying"),
                format!("publish error. Could not confirm restoration of the previous file: rollback error. Check the original destination {target} and the possible backup {backup} before retrying"),
            ),
            (
                false,
                format!("publish error. Не удалось восстановить прежний файл: rollback error. Перед повтором проверьте сохранённую версию в {backup}"),
                format!("publish error. Не удалось подтвердить восстановление прежнего файла: rollback error. Перед повтором проверьте исходное назначение {target} и возможную резервную копию {backup}"),
            ),
        ] {
            let local = format_replacement_rollback_failed(
                english, "publish error", "rollback error", target, backup, false,
            );
            let remote = format_replacement_rollback_failed(
                english, "publish error", "rollback error", target, backup, true,
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
                format_initial_backup_rename_failed(english, "denied", target, backup, false);
            let remote =
                format_initial_backup_rename_failed(english, "denied", target, backup, true);
            assert_eq!(local, local_guidance);
            assert_eq!(remote, remote_guidance);
            assert!(!local.contains(backup));
            assert!(remote.contains(target) && remote.contains(backup));
        }
    }

    #[test]
    fn source_change_error_explains_safe_retry_in_both_languages() {
        let source = "/tmp/Документы {name}.txt";
        assert_eq!(
            format_source_changed(true, source),
            format!("Source “{source}” changed during copying; the copy was not published. Try again after writing to the source has finished")
        );
        assert_eq!(
            format_source_changed(false, source),
            format!("Источник «{source}» изменился во время копирования; копия не опубликована. Повторите попытку после завершения записи в источник")
        );
    }

    #[test]
    fn catalog_entries_have_unique_russian_keys_and_nonempty_translations() {
        let mut seen = std::collections::HashSet::new();
        for message in catalog::MESSAGES {
            assert!(!message.russian.trim().is_empty());
            assert!(!message.english.trim().is_empty(), "{}", message.russian);
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
            assert_eq!(key.translate(true), en);
            assert_eq!(key.translate(false), ru);
        }
    }
}
