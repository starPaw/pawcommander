//! Interface translations. Filesystem paths and user content never pass through this catalog.
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};

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

pub fn text(russian: &'static str) -> &'static str {
    translate(is_english(), russian)
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

fn translate(english: bool, russian: &'static str) -> &'static str {
    if !english {
        return russian;
    }
    match russian {
        "Не удалось проверить источник" => "Could not check source",
        "Не удалось скопировать" => "Could not copy",
        "Совпадение имён" => "Name conflict",
        "Совпадение {} из {}" => "Conflict {} of {}",
        "Исходный объект" => "Source",
        "Существующий объект" => "Existing item",
        "Замена доступна только для двух разных обычных файлов." => "Replacement is available only for two different regular files.",
        "Применить к остальным совпадениям" => "Apply to remaining conflicts",
        "Отменить операцию" => "Cancel operation",
        "Пропустить" => "Skip",
        "Сохранить обе версии" => "Keep both",
        "Заменить" => "Replace",
        "{bytes} Б" => "{bytes} B",
        "{} · Размер {size} · Изменён {date}" => "{} · Size {size} · Modified {date}",
        "Назад" => "Back",
        "Вперёд" => "Forward",
        "Родительская папка" => "Parent folder",
        "Обновить" => "Refresh",
        "Поиск файлов" => "Search files",
        "Архив открыт только для просмотра" => "Archive is open for browsing only",
        "Чтение архива…" => "Reading archive…",
        "Чтение архива отменено" => "Archive read cancelled",
        "Поддерживаются архивы ZIP и TAR" => "ZIP and TAR archives are supported",
        "Папка отсутствует в архиве" => "Folder is missing from the archive",
        "Путь в архиве занят и папкой, и файлом" => "The same archive path is used for a folder and a file",
        "Недопустимая запись архива" => "Invalid archive entry",
        "Недопустимый путь внутри архива" => "Invalid path inside the archive",
        "Недопустимый адрес архива" => "Invalid archive address",
        "Слишком много данных для просмотра архива" => "Archive contains too much metadata to browse",
        "Архив должен быть обычным файлом" => "Archive must be a regular file",
        "Создать папку" => "Create folder",
        "Копировать" => "Copy",
        "Вставить" => "Paste",
        "Сетка" => "Grid",
        "Колоночный режим" => "Column view",
        "Список" => "List",
        "Две панели" => "Dual pane",
        "Расположение панелей" => "Pane layout",
        "Сортировка" => "Sort",
        "Вид текущей папки" => "Current folder view",
        "Плотность" => "Density",
        "Обычная" => "Standard",
        "Компактная" => "Compact",
        "Рядом" => "Side by side",
        "Одна над другой" => "Top and bottom",
        "Боковая панель" => "Sidebar",
        "Скрыть боковую панель" => "Hide sidebar",
        "Показать боковую панель" => "Show sidebar",
        "Дерево папок" => "Folder tree",
        "Раскрыть папку" => "Expand folder",
        "Свернуть папку" => "Collapse folder",
        "Чтение вложенных папок…" => "Reading subfolders…",
        "Повторить чтение папки" => "Retry reading folder",
        "Для просмотра дерева подключитесь по SFTP." => "Connect over SFTP to browse the tree.",
        "Сведения" => "Details",
        "Действие" => "Action",
        "Перетащите файлы с диска: у части объектов нет локального пути" => "Drag files from disk: some items have no local path",
        "Перетащите на папку или в область файлов панели" => "Drop onto a folder or a pane's file area",
        "Дождитесь окончания чтения папки" => "Wait for the folder to finish loading",
        "Переместить" => "Move",
        " · Shift: переместить · Esc: отмена" => " · Shift: move · Esc: cancel",
        "Между подключениями используйте копирование: отпустите Shift" => "Use copy between connections: release Shift",
        "Нельзя переносить элемент в себя или вложенную папку" => "Cannot move an item into itself or a subfolder",
        "Элементы уже находятся в этой папке" => "Items are already in this folder",
        "Подсчёт не запущен" => "Calculation not started",
        "Свойства локальных файлов" => "Local file properties",
        "Остановка подсчёта…" => "Stopping calculation…",
        "Подсчёт…" => "Calculating…",
        "Остановить подсчёт" => "Stop calculation",
        "Подсчёт остановлен — частичный результат" => "Calculation stopped — partial result",
        "Подсчёт завершён с ошибками — частичный результат" => "Calculation completed with errors — partial result",
        "Подсчёт завершён" => "Calculation complete",
        "Папка изменилась во время подсчёта" => "Folder changed during calculation",
        "Размер файлов: {} байт" => "File size: {} bytes",
        "Файлов: {} · Папок: {} · Ссылок: {} · Других: {}" => "Files: {} · Folders: {} · Links: {} · Other: {}",
        "Включая выбранные объекты и скрытые файлы. Ссылки не раскрываются. Размер — сумма длин обычных файлов, не занятое место на диске." => "Includes selected items and hidden files. Links are not followed. Size is the sum of regular file lengths, not disk usage.",
        "Ошибок чтения: {}" => "Read errors: {}",
        "недоступно" => "unavailable",
        "Только чтение: {}" => "Read-only: {}",
        "Права: {permissions}\nИзменён: {}\nСоздан: {}\nДоступ: {}" => "Permissions: {permissions}\nModified: {}\nCreated: {}\nAccessed: {}",
        "Не удалось определить профиль SFTP" => "Could not identify the SFTP profile",
        "Системное хранилище секретов недоступно: {error}. Ключ и фраза не записываются в настройки." => "System credential store unavailable: {error}. The key and passphrase are not written to settings.",
        "Сохранённый ключ повреждён; удалите его и сохраните заново" => "Saved key is corrupt; remove it and save it again",
        "Не удалось подготовить ключ к сохранению" => "Could not prepare the key for storage",
        "Как в системе" => "Follow system",
        "Мягкие цвета для ваших файлов" => "Soft colors for your files",
        "Служба предпросмотра недоступна" => "Preview service unavailable",
        "СВЕДЕНИЯ" => "DETAILS",
        "Обновить предпросмотр" => "Refresh preview",
        "Ничего не выбрано" => "Nothing selected",
        "Выберите файл или папку,\nчтобы увидеть подробности." => "Select a file or folder\nto see its details.",
        "Загрузка сведений…" => "Loading details…",
        "Размер" => "Size",
        "Изменён" => "Modified",
        "Разрешение" => "Dimensions",
        "Расположение" => "Location",
        "ПРЕДПРОСМОТР" => "PREVIEW",
        "Текст" => "Text",
        "Форматированный" => "Formatted",
        "Исходный текст" => "Source",
        "[Изображение: " => "[Image: ",
        "Пассивный просмотр: ссылки неактивны, изображения заменены текстом. Скрипты, стили и внешние ресурсы отключены." => "Passive preview: links are inert and images use text placeholders. Scripts, styles and external resources are disabled.",
        "Форматированный просмотр сокращён из-за ограничения сложности. Исходный текст доступен отдельно." => "Formatted preview shortened by the complexity limit. Source remains available separately.",
        "Найти в предпросмотре" => "Find in preview",
        "Предыдущее совпадение" => "Previous match",
        "Следующее совпадение" => "Next match",
        "Показано начало файла: до 32 КиБ, 1000 строк и 8 КиБ на строку." => "Showing the file prefix: up to 32 KiB, 1000 lines and 8 KiB per line.",
        "Пустой файл" => "Empty file",
        "Показано начало файла (до 32 КиБ)." => "Showing the start of the file (up to 32 KiB).",
        "Папка SFTP" => "SFTP folder",
        "Файл SFTP" => "SFTP file",
        "Недоступно" => "Unavailable",
        "Не удалось прочитать сведения: {e}" => "Could not read details: {e}",
        "Не удалось открыть объект: {e}" => "Could not open item: {e}",
        "Ссылка на папку" => "Link to folder",
        "Ссылка на файл" => "Link to file",
        "Папка" => "Folder",
        "Файл" => "File",
        "Ссылка" => "Link",
        "Специальный объект" => "Special item",
        "Символическая ссылка" => "Symbolic link",
        "Левая панель" => "Left pane",
        "Правая панель" => "Right pane",
        "Верхняя панель" => "Top pane",
        "Нижняя панель" => "Bottom pane",
        "Фильтр по имени" => "Filter by name",
        "Имя файла" => "File name",
        "Запрос поиска" => "Search query",
        "Специальный файл" => "Special file",
        "Предпросмотр специальных файлов недоступен." => "Preview is unavailable for special files.",
        "Изображение больше 20 МиБ. Откройте его во внешнем приложении." => "Image exceeds 20 MiB. Open it in an external application.",
        "Изображение больше 20 МиБ." => "Image exceeds 20 MiB.",
        "Не удалось определить формат изображения: {e}" => "Could not detect image format: {e}",
        "Предпросмотр изображения недоступен: {e}" => "Image preview unavailable: {e}",
        "Не удалось прочитать файл: {e}" => "Could not read file: {e}",
        "Предпросмотр доступен для текста UTF-8 и изображений PNG, JPEG, GIF, WebP." => "Preview supports UTF-8 text and PNG, JPEG, GIF, WebP images.",
        "Для этого формата предпросмотр недоступен." => "Preview is unavailable for this format.",
        "Б" => "B",
        "КиБ" => "KiB",
        "МиБ" => "MiB",
        "ГиБ" => "GiB",
        "ТиБ" => "TiB",
        "{value:.1} {} ({bytes} Б)" => "{value:.1} {} ({bytes} B)",
        "Zed не найден. Установите Zed; при нестандартной установке выполните в Zed команду «cli: install cli binary» и обновите проверку в настройках PawCommander." => "Zed not found. Install Zed; for a custom installation, run “cli: install cli binary” in Zed and check again in PawCommander settings.",
        "Не удалось проверить текстовый SFTP-файл: {e}" => "Could not check the SFTP text file: {e}",
        "SFTP-файл не распознан как текстовый. Используйте «Открыть в Zed» в контекстном меню для явного открытия или скопируйте файл локально." => "SFTP file was not recognized as text. Choose “Open in Zed” from the context menu to open it explicitly, or copy it locally.",
        "Запрос открытия передан Zed. Подключение и SSH-аутентификация выполняются в Zed." => "Open request sent to Zed. Zed handles the connection and SSH authentication.",
        "Запрос открытия передан Zed." => "Open request sent to Zed.",
        "Путь SFTP должен быть UTF-8" => "SFTP path must be UTF-8",
        "Для Zed нужен абсолютный POSIX-путь SFTP" => "Zed requires an absolute POSIX SFTP path",
        "Некорректный сервер SSH для Zed" => "Invalid SSH host for Zed",
        "Укажите пользователя и порт SSH 1–65535" => "Specify an SSH user and port 1–65535",
        "Некорректный пользователь SSH для Zed" => "Invalid SSH user for Zed",
        "Некорректный порт SSH для Zed" => "Invalid SSH port for Zed",
        "Некорректный адрес SSH для Zed" => "Invalid SSH address for Zed",
        "Не удалось запустить Zed: {e}" => "Could not start Zed: {e}",
        "Zed не принял запрос открытия ({status}). Проверьте установку Zed и SSH-подключение в Zed." => "Zed rejected the open request ({status}). Check the Zed installation and its SSH connection.",
        "Файловый буфер содержит путь без имени или относительный путь" => "File clipboard contains an unnamed or relative path",
        "Файловый буфер содержит недопустимый или нелокальный URL" => "File clipboard contains an invalid or non-local URL",
        "Системный файловый буфер недоступен на этой платформе" => "System file clipboard is unavailable on this platform",
        "Не удалось прочитать папку" => "Could not read folder",
        "Не удалось прочитать элемент папки" => "Could not read folder entry",
        "Не удалось получить сведения об элементе" => "Could not get item details",
        "Не удалось создать папку" => "Could not create folder",
        "Нельзя переименовать корень файловой системы" => "Cannot rename the filesystem root",
        "Нельзя скопировать или переместить «{}» в себя или во вложенную папку" => "Cannot copy or move “{}” into itself or a subfolder",
        "Нельзя скопировать или переместить корень файловой системы" => "Cannot copy or move the filesystem root",
        "Несколько выбранных элементов называются «{}»; перенесите их по отдельности" => "Multiple selected items are named “{}”; transfer them separately",
        "Задача поиска завершилась неожиданно" => "Search task ended unexpectedly",
        "Сохранённые поиски" => "Saved searches",
        "Сохранить поиск…" => "Save search…",
        "Название поиска" => "Search name",
        "Переименовать поиск" => "Rename search",
        "Поиск с таким названием уже сохранён." => "A search with this name already exists.",
        "Удалить сохранённый поиск" => "Remove saved search",
        "Путь сохранённого поиска несовместим с системой." => "This saved search uses a path incompatible with this system.",
        "В папке и подпапках" => "Folder and subfolders",
        "Фильтры" => "Filters",
        "Фильтры (активны)" => "Filters (active)",
        "Тип объекта" => "Entry type",
        "Любой тип" => "Any type",
        "Обычные файлы" => "Regular files",
        "Только папки" => "Folders",
        "Ссылки и прочие" => "Links and other",
        "например txt" => "e.g. txt",
        "Минимальный размер" => "Minimum size",
        "Максимальный размер" => "Maximum size",
        "Единица размера" => "Size unit",
        "Изменено с" => "Modified from",
        "Изменено по" => "Modified through",
        "Максимальная глубина" => "Maximum depth",
        "1 = прямые потомки" => "1 = direct children",
        "Размер и расширение относятся к обычным файлам. Границы включены. Даты местные." => "Size and extension apply to regular files. Bounds are inclusive. Dates use local time.",
        "Сбросить фильтры" => "Reset filters",
        "Имя необязательно при заданных фильтрах" => "Name is optional when filters are set",
        "Индекс locate" => "Locate index",
        "Проверка пути из индекса…" => "Checking indexed path…",
        "Предыдущая проверка пути ещё ждёт ответа файловой системы." => "The previous path check is still waiting for the filesystem.",
        "Файловая система не ответила на проверку пути из индекса." => "The filesystem did not respond to the indexed path check.",
        "Исключения поиска" => "Search exclusions",
        "Исключить имена через запятую, например .git, target, *.tmp" => "Comma-separated names or patterns, e.g. .git, target, *.tmp",
        "Для поиска в папке откройте локальную папку в панели." => "Open a local folder in a pane to search it.",
        "Часть имени или маска: отчёт, *.rs, фото?.jpg" => "Name fragment or pattern: report, *.rs, photo?.jpg",
        "Текст файла" => "File text",
        "Регулярное выражение" => "Regular expression",
        "Текст для поиска, например TODO" => "Text to find, e.g. TODO",
        "Регулярное выражение, например TODO\\s*:" => "Regular expression, e.g. TODO\\s*:",
        "Поиск по строкам в файлах UTF-8 до 1 МиБ, с учётом регистра по умолчанию." => "Search line by line in UTF-8 files up to 1 MiB. Case-sensitive by default.",
        "При поиске по выражению строки больше 64 КиБ пропускаются." => "Regular expression search skips lines longer than 64 KiB.",
        "Остановка поиска…" => "Stopping search…",
        "В просмотренных папках совпадений нет." => "No matches in the scanned folders.",
        "В просмотренных файлах совпадений нет." => "No matches in the scanned files.",
        "Укоротите запрос или список исключений." => "Shorten the query or exclusion list.",
        "Поиск пропустил слишком глубоко вложенные папки." => "Some folders were skipped because they are nested too deeply.",
        "Поиск по полному пути в локальном индексе, без учёта регистра." => "Case-insensitive full-path search in the local index.",
        "База может отставать от файловой системы; SFTP не индексируется." => "The index may lag behind the filesystem; SFTP is not indexed.",
        "locate не найден" => "locate not found",
        "На Linux установите locate/plocate; на macOS используется /usr/bin/locate. Затем: Настройки → Инструменты → Проверить снова." => "On Linux, install locate/plocate; macOS uses /usr/bin/locate. Then choose Settings → Tools → Check again.",
        "Часть пути или маска: Cargo, *.rs, */Documents/*" => "Path fragment or pattern: Cargo, *.rs, */Documents/*",
        "Найти" => "Search",
        "Отмена" => "Cancel",
        "Поиск отменён" => "Search cancelled",
        "Если база отсутствует или устарела, обновите её средствами ОС (locate.updatedb / updatedb)." => "If the index is missing or outdated, update it using OS tools (locate.updatedb / updatedb).",
        "+ · Уточните запрос, показаны первые 1000" => "+ · Refine your query; showing the first 1000",
        "Совпадений в индексе нет. Проверьте запрос и актуальность базы locate." => "No index matches. Check your query and whether the locate index is up to date.",
        "Показать в панели" => "Show in pane",
        "Показать результаты отдельной панелью" => "Show in results pane",
        "Результаты поиска" => "Search results",
        "Закрыть результаты поиска" => "Close search results",
        "Показать в активной панели" => "Show in active pane",
        "Проверка результатов поиска…" => "Checking search results…",
        "Открытие файла…" => "Opening file…",
        "Файловая система не ответила на проверку результатов поиска." => "The filesystem did not respond to the search results check.",
        "Проверка результатов поиска завершилась неожиданно." => "The search results check ended unexpectedly.",
        "Активная папка или вкладка изменилась. Повторите действие." => "The active folder or tab changed. Try the action again.",
        "Закройте открытое окно или меню и повторите действие." => "Close the open dialog or menu and try the action again.",
        "Дождитесь завершения текущей файловой операции и повторите копирование." => "Wait for the current file operation to finish, then try copying again.",
        "Двойной щелчок — перейти к результату в активной панели." => "Double-click to reveal the result in the active pane.",
        "Превышен лимит вывода locate; уточните запрос" => "locate output limit exceeded; refine your query",
        "Введите непустой запрос без NUL" => "Enter a non-empty query without NUL",
        "Не удалось запустить locate: {e}" => "Could not start locate: {e}",
        "locate не ответил за 15 секунд; проверьте базу индекса" => "locate did not respond within 15 seconds; check its index",
        "Ошибка ожидания locate: {e}" => "Error waiting for locate: {e}",
        "Ошибка чтения вывода locate" => "Could not read locate output",
        "Ошибка чтения ошибок locate" => "Could not read locate errors",
        "Ошибка locate ({status}). {warning}\nПроверьте наличие и актуальность базы locate." => "locate error ({status}). {warning}\nCheck that the locate index exists and is up to date.",
        "locate вернул путь не в UTF-8" => "locate returned a non-UTF-8 path",
        "Не удалось запустить чтение папки: {error}" => "Could not start reading folder: {error}",
        "Результат поиска уже отсутствует в папке. Выполните поиск снова." => "The search result no longer exists in this folder. Run the search again.",
        "Автообновление: {error}. Показан последний прочитанный список; повторим попытку автоматически." => "Auto refresh: {error}. Showing the last successful listing; will retry automatically.",
        "Не удалось прочитать папку: фоновая задача завершилась" => "Could not read folder: background task ended",
        "Не удалось запустить операцию: {error}" => "Could not start operation: {error}",
        "Диалог конфликта закрыт" => "Conflict dialog closed",
        "Остановить" => "Stop",
        "Остановка…" => "Stopping…",
        "Остановить перенос · Shift+Esc" => "Stop transfer · Shift+Esc",
        "Перемещение файлов" => "Moving files",
        "Копирование файлов" => "Copying files",
        "Открытие файла в Zed…" => "Opening file in Zed…",
        "Задача открытия в Zed завершилась неожиданно" => "Zed open task ended unexpectedly",
        "Задача подключения завершилась" => "Connection task ended",
        "Фоновая операция неожиданно завершилась. Проверьте файлы в обеих папках." => "Background operation ended unexpectedly. Check the files in both folders.",
        "{} объектов в системном файловом буфере" => "{} items in the system file clipboard",
        "Системный буфер недоступен: {error}. Используется буфер PawCommander." => "System clipboard unavailable: {error}. Using the PawCommander clipboard.",
        "{} объектов в буфере PawCommander (SFTP)" => "{} items in the PawCommander clipboard (SFTP)",
        "Не удалось прочитать системный файловый буфер: {error}" => "Could not read system file clipboard: {error}",
        "В файловом буфере нет объектов для вставки" => "No items to paste in the file clipboard",
        "Не удалось открыть файл: {e}" => "Could not open file: {e}",
        "Перемещение в соседнюю панель" => "Moving to the other pane",
        "Копирование в соседнюю панель" => "Copying to the other pane",
        "SFTP нельзя показать в системном файловом менеджере" => "SFTP cannot be revealed in the system file manager",
        "Не удалось показать файл: {e}" => "Could not reveal file: {e}",
        "Поиск файлов…" => "Search files…",
        "Поиск…" => "Search…",
        "Подключиться по SFTP" => "Connect via SFTP",
        "Копировать · ⌘/Ctrl+C" => "Copy · ⌘/Ctrl+C",
        "Вставить · ⌘/Ctrl+V" => "Paste · ⌘/Ctrl+V",
        "Настройки" => "Settings",
        "Доступность" => "Accessibility",
        "Высокая контрастность" => "High contrast",
        "Масштаб интерфейса" => "Interface scale",
        "Фокус на панели инструментов" => "Focus toolbar",
        "Закрыть" => "Close",
        "Двойной щелчок — открыть" => "Double-click to open",
        "Ввести путь вручную" => "Enter path manually",
        "Свойства…" => "Properties…",
        "Свойства" => "Properties",
        "Действующие сочетания показаны в разделе Настройки → Горячие клавиши." => "See active bindings in Settings → Keyboard Shortcuts.",
        "Горячие клавиши" => "Keyboard Shortcuts",
        "Палитра команд" => "Command Palette",
        "Быстрый переход к папке" => "Go to Folder",
        "К папке…" => "Go to Folder…",
        "Имя или путь папки…" => "Folder name or path…",
        "Недавних папок пока нет" => "No recent folders yet",
        "Папки не найдены" => "No matching folders",
        "Порядок зависит от давности и частоты посещений" => "Ranked by how recently and often you opened each folder",
        "Сначала лучшие совпадения, затем давность и частота посещений" => "Best matches first, then recent and frequent folders",
        "Поиск команд…" => "Search commands…",
        "Команды не найдены" => "No matching commands",
        "Вверх/вниз — выбрать · Enter — выполнить · Esc — закрыть" => "Up/Down to select · Enter to run · Esc to close",
        "Закрыть вкладку" => "Close tab",
        "Закрепить вкладку" => "Pin tab",
        "Открепить вкладку" => "Unpin tab",
        "Закрепить или открепить вкладку" => "Pin or unpin tab",
        "Дублировать вкладку" => "Duplicate tab",
        "Восстановить закрытую вкладку" => "Reopen closed tab",
        "Переключить панель" => "Switch panes",
        "Копировать в соседнюю панель" => "Copy to the other pane",
        "Переместить в соседнюю панель" => "Move to the other pane",
        "Стандартный" => "Default",
        "Это сочетание зарезервировано. Выберите другое." => "This shortcut is reserved. Choose another.",
        "Профиль" => "Profile",
        "Сбросить настройки клавиш" => "Reset to defaults",
        "Смена профиля или сброс очищает изменения клавиш." => "Changing the profile or resetting clears custom bindings.",
        "Нажмите на клавишу действия для записи сочетания. Esc отменяет запись." => "Click a binding to record a shortcut. Press Esc to cancel recording.",
        "Нажмите сочетание…" => "Press a shortcut…",
        "Не назначено" => "Unassigned",
        "Очистить" => "Clear",
        "Системные сочетания буфера обмена, стрелки ↑/↓ и Esc закреплены." => "Clipboard shortcuts, Up/Down navigation, and Esc use fixed bindings.",
        "Скрытые файлы" => "Hidden files",
        "Инструменты" => "Tools",
        "{name}: найден" => "{name}: found",
        "{name}: не найден" => "{name}: not found",
        "locate: установите locate/plocate и создайте индекс средствами ОС." => "locate: install locate/plocate and create an index using OS tools.",
        "Zed: установите редактор; для нестандартного пути — cli: install cli binary." => "Zed: install the editor; for a custom path, run cli: install cli binary.",
        "Проверить снова" => "Check again",
        "Панель сведений" => "Details panel",
        "Быстрый просмотр" => "Quick Look",
        "Предыдущий файл" => "Previous File",
        "Следующий файл" => "Next File",
        "Ваши файлы" => "Your files",
        "Домашняя" => "Home",
        "Рабочий стол" => "Desktop",
        "Документы" => "Documents",
        "Загрузки" => "Downloads",
        "Изображения" => "Pictures",
        "Музыка" => "Music",
        "Видео" => "Videos",
        "Подключения" => "Connections",
        "SFTP · Подключиться" => "SFTP · Connect",
        "Пространства" => "Workspaces",
        "Сохранить пару папок…" => "Save folder pair…",
        "Сохранить пару папок" => "Save folder pair",
        "Переименовать рабочее пространство" => "Rename workspace",
        "Удалить пространство" => "Remove workspace",
        "Имя рабочего пространства" => "Workspace name",
        "Левая папка" => "Left folder",
        "Правая папка" => "Right folder",
        "Это имя уже используется." => "This name is already in use.",
        "Пути этого рабочего пространства несовместимы с системой." => "This workspace contains paths that are incompatible with this system.",
        "Избранное" => "Favorites",
        "Изменить избранное" => "Edit favorite",
        "Сочетание избранного очищено: клавиша занята другой командой" => "Favorite shortcut cleared because another command uses that key",
        "Название" => "Name",
        "Группа" => "Group",
        "Сочетание" => "Shortcut",
        "Вверх" => "Move up",
        "Вниз" => "Move down",
        "Убрать из избранного" => "Remove from favorites",
        "+ Текущая папка" => "+ Current folder",
        "Устройства" => "Devices",
        "Файловая система" => "Filesystem",
        "Тома и диски" => "Volumes and disks",
        "Носители" => "Media",
        "Диск {}:" => "Drive {}:",
        "Файловые операции: {} · {} с" => "File operations: {} · {} s",
        "{} объектов  ·  Выбрано: {}" => "{} items  ·  Selected: {}",
        "Подключение SFTP" => "SFTP connection",
        "Сохранённые подключения" => "Saved connections",
        "Сервер" => "Host",
        "Порт" => "Port",
        "Пользователь" => "User",
        "Начальная папка" => "Initial folder",
        "Пароль" => "Password",
        "Приватный ключ" => "Private key",
        "Путь к ключу (необязательно)" => "Key path (optional)",
        "Пароль ключа" => "Key passphrase",
        "Использовать сохранённый ключ и фразу" => "Use saved key and passphrase",
        "Сохранить ключ и фразу после подключения" => "Save key and passphrase after connecting",
        "Ключ и его фраза сохраняются в системном хранилище секретов. Пароль аккаунта не сохраняется. При сохранении читается выбранный файл ключа. ~/.ssh/config не читается." => "The key and passphrase are saved in the system credential store. The account password is not saved. Saving reads the selected key file. ~/.ssh/config is not read.",
        "Удалить сохранённый секрет" => "Remove saved secret",
        "Ключ сервера должен быть проверен и сохранён в ~/.ssh/known_hosts через OpenSSH." => "Verify and save the host key in ~/.ssh/known_hosts using OpenSSH first.",
        "Подключиться" => "Connect",
        "Подключение…" => "Connecting…",
        "Удаление сохранённого секрета" => "Removing saved secret",
        "Сохранённый ключ и фраза удалены. Исходный файл ключа и открытое SSH-соединение сохранены." => "Saved key and passphrase removed. The original key file and active SSH connection are preserved.",
        "О PawCommander" => "About PawCommander",
        "Ваши файлы. Две панели." => "Your files. Two panes.",
        "Файловый менеджер на Rust, вдохновлённый Dolphin." => "A Rust file manager inspired by Dolphin.",
        "Ввести путь" => "Enter path",
        "Фильтр текущей папки" => "Filter current folder",
        "Выделить всё" => "Select all",
        "Выделить по маске" => "Select by mask",
        "Выделить по расширению" => "Select by extension",
        "Инвертировать выделение" => "Invert selection",
        "Восстановить выделение" => "Restore selection",
        "Маска имени файла" => "Filename mask",
        "Звёздочка * заменяет любое число символов, знак ? — один символ." => "* matches any number of characters and ? matches one character.",
        "Длина маски не должна превышать 1024 байта." => "The mask can be up to 1024 bytes.",
        "Введите одно расширение, например txt или .txt." => "Enter one extension, such as txt or .txt.",
        "Выделить" => "Select",
        "Поиск подходящих объектов…" => "Finding matching items…",
        "Папка или список изменились. Повторите выделение." => "The folder or its visible items changed. Try the selection again.",
        "Нет подходящих видимых объектов" => "No matching visible items",
        "В корзину с подтверждением" => "Move to trash with confirmation",
        "Для локальных файлов используется системный буфер, если он доступен. Для SFTP используется буфер PawCommander." => "Local files use the system clipboard when available. SFTP files use the PawCommander clipboard.",
        "Вставка файлов срабатывает при нажатии V. Отпускайте Cmd и Option в любом порядке. В текстовых полях работают обычные Cmd+C, Cmd+X и Cmd+V." => "File paste starts when V is pressed. Release Cmd and Option in any order. Text fields keep the usual Cmd+C, Cmd+X and Cmd+V shortcuts.",
        "При совпадении имён выберите действие в диалоге. Удаление SFTP — навсегда с подтверждением; локальные файлы отправляются в корзину." => "Choose an action in the name conflict dialog. SFTP deletion is permanent and requires confirmation; local files go to the trash.",
        "Новая папка" => "New folder",
        "Переименовать" => "Rename",
        "Удалить с сервера навсегда?" => "Permanently delete from server?",
        "Отправить в корзину?" => "Move to trash?",
        "Имя папки" => "Folder name",
        "Выбрано объектов: {}" => "Selected items: {}",
        "…и ещё {}" => "…and {} more",
        "Файлы и содержимое папок будут удалены с сервера без возможности восстановления из корзины." => "Files and folder contents will be deleted from the server and cannot be restored from the trash.",
        "Их можно будет восстановить из системной корзины." => "You can restore them from the system trash.",
        "Удалить навсегда" => "Delete permanently",
        "В корзину" => "Move to trash",
        "Сохранить" => "Save",
        "Создание папки" => "Creating folder",
        "Папка создана: {}" => "Folder created: {}",
        "Переименование" => "Renaming",
        "Переименовано: {}" => "Renamed: {}",
        "Удаление файлов" => "Deleting files",
        "Обработано объектов: {n}" => "Items processed: {n}",
        "Файлы переданы принимающему приложению" => "Files handed to the receiving application",
        "Дождитесь завершения файловой операции, затем закройте окно." => "Wait for the file operation to finish before closing the window.",
        "Закрыть вкладку {name}" => "Close tab {name}",
        "Новая вкладка" => "New tab",
        "Вернуться к хлебным крошкам · Esc" => "Return to breadcrumbs · Esc",
        "Путь к папке или sftp://сервер/папка" => "Folder path or sftp://host/folder",
        "Путь к папке" => "Folder path",
        "Подсказки пути" => "Folder suggestions",
        "↑↓ выбрать · Tab дополнить · Enter открыть" => "↑↓ select · Tab complete · Enter open",
        "Перейти по введённому пути · Enter" => "Go to entered path · Enter",
        "Домой" => "Home",
        "Диск" => "Drive",
        "Перейти в {}" => "Go to {}",
        "Фильтр по имени…" => "Filter by name…",
        "Имя" => "Name",
        "Столбцы списка" => "List columns",
        "Расширение" => "Extension",
        "Права" => "Permissions",
        "Владелец" => "Owner",
        "Создан" => "Created",
        "Последний доступ" => "Last accessed",
        "Сбросить столбцы" => "Reset columns",
        "Переместить выше" => "Move up",
        "Переместить ниже" => "Move down",
        "Ширина столбца" => "Column width",
        "Сортировка: {label}" => "Sort: {label}",
        "Чтение папки…" => "Reading folder…",
        "Проверять копии (SHA-256)" => "Verify copies (SHA-256)",
        "Проверка SHA-256" => "SHA-256 verification",
        "Повторно читает временные файлы перед публикацией. Применяется к новым заданиям копирования; перемещение использует переименование." => "Reads back staged files before publication. Applies to newly queued copies; moves use rename.",
        "Папка пуста" => "Folder is empty",
        "Ничего не найдено" => "No results",
        "Создайте папку или вставьте файлы" => "Create a folder or paste files",
        "Попробуйте другое имя" => "Try a different name",
        "Выбрано: {}" => "Selected: {}",
        "{} объектов" => "{} items",
        "Открыть" => "Open",
        "Открыть в Zed" => "Open in Zed",
        "Zed не найден. Установите редактор и выберите Настройки → Инструменты → Проверить снова." => "Zed not found. Install the editor and choose Settings → Tools → Check again.",
        "В соседнюю панель" => "To the other pane",
        "Вырезать" => "Cut",
        "Переименовать…" => "Rename…",
        "Удалить навсегда…" => "Delete permanently…",
        "В корзину…" => "Move to trash…",
        "Копировать путь" => "Copy path",
        "Показать в системе" => "Show in system file manager",
        "Ожидается решение" => "Waiting for a decision",
        "Текущий объект" => "Current item",
        "Журнал результатов" => "Result history",
        "Отмена изменений" => "Undo changes",
        "Undo недоступен для этой операции" => "Undo is unavailable for this operation",
        "Очередь заполнена (100 заданий). Дождитесь завершения или удалите ожидающее задание." => "Queue is full (100 jobs). Wait for completion or remove a pending job.",
        "Эти вырезанные объекты уже ожидают перемещения." => "These cut items are already scheduled to move.",
        "Очередь: {} · одновременно: 1" => "Queued: {} · concurrency: 1",
        "Остановка действует только на текущее задание. Ожидающие задания хранятся до закрытия приложения." => "Stop affects only the current job. Pending jobs last for this session only.",
        "Раньше" => "Earlier",
        "Позже" => "Later",
        "Удалить из очереди" => "Remove from queue",
        "Повторить запуск очереди" => "Retry queue",
        "История не сохранена" => "History not saved",
        "История не сохранена. Проверьте файл журнала и доступ к нему." => "History not saved. Check the history file and its permissions.",
        "Копии раскрываются по манифесту; остальные результаты могут относиться только к выбранному объекту." => "Copies expand from their manifest; other results may cover only the selected item.",
        "До 100 операций и 100 000 строк; объём истории ограничен. Неполные записи помечены." => "Up to 100 operations and 100,000 rows; history size is bounded. Incomplete records are marked.",
        "Только выбранный объект; потомки не перечислены" => "Selected item only; descendants not enumerated",
        "Пропущено вместе с выбранным элементом; отдельный перенос не начинался" => "Skipped together with the selected item; no separate transfer was started",
        "Публикация не подтверждена" => "Publication unconfirmed",
        "Длинные пути или сообщения сокращены из-за лимита журнала." => "Long paths or messages were shortened by history limits.",
        "Журнал пуст" => "No results yet",
        "Загрузка журнала…" => "Loading history…",
        "Завершено" => "Completed",
        "Завершено с предупреждением" => "Completed with warning",
        "Пропущено" => "Skipped",
        "Ошибка" => "Failed",
        "Отменено" => "Cancelled",
        "Не начато" => "Not started",
        "Пропущено при разрешении конфликта" => "Skipped by conflict decision",
        "Операция остановлена пользователем" => "Operation stopped by user",
        "Не начато после ошибки; причина указана в итоге операции" => "Not started after an error; see the operation result for the reason",
        "Записано: {written}" => "Written: {written}",
        "{}/с" => "{}/s",
        "{seconds} с" => "{seconds} s",
        "{} мин {:02} с" => "{} min {:02} s",
        "До конца файла: ≈ {time}" => "File remaining: ≈ {time}",
        "Завершение записи…" => "Finishing write…",
        "Записано за операцию: {}" => "Total written: {}",
        "{:.1} КБ" => "{:.1} KB",
        "{:.1} МБ" => "{:.1} MB",
        "{:.1} ГБ" => "{:.1} GB",
        "Некорректный адрес SFTP: {e}" => "Invalid SFTP address: {e}",
        "Используйте sftp://пользователь@сервер:22/путь без пароля, query и fragment" => "Use sftp://user@host:22/path without a password, query or fragment",
        "Укажите сервер SFTP" => "Specify an SFTP host",
        "Имя пользователя должно быть UTF-8" => "Username must be UTF-8",
        "Укажите пользователя: sftp://пользователь@сервер/путь" => "Specify a user: sftp://user@host/path",
        "Путь должен быть UTF-8" => "Path must be UTF-8",
        "Недопустимый путь" => "Invalid path",
        "Для домашней папки сервера используйте кнопку SFTP и начальный путь «.»" => "For the server's home folder, use the SFTP button and initial path “.”",
        "Используйте абсолютный POSIX-путь SFTP без обратных слешей" => "Use an absolute POSIX SFTP path without backslashes",
        "Путь содержит нулевой байт" => "Path contains a null byte",
        "Не удалось записать файлы в буфер Finder" => "Could not write files to the Finder clipboard",
        "Системный буфер изменился. Повторите вставку." => "System clipboard changed. Paste again.",
        "Файловый буфер содержит недопустимый URL" => "File clipboard contains an invalid URL",
        "Не удалось найти файл из буфера Finder: ссылка устарела или файл недоступен" => "Could not find file from Finder clipboard: stale reference or unavailable file",
        "Окно PawCommander недоступно для записи в буфер Windows" => "PawCommander window unavailable for writing to the Windows clipboard",
        "Не удалось зарегистрировать Preferred DropEffect" => "Could not register Preferred DropEffect",
        "Путь содержит нулевой символ" => "Path contains a null character",
        "Список файлов в буфере слишком велик" => "Clipboard file list is too large",
        "Путь нельзя записать в буфер как файловый URL" => "Path cannot be written to the clipboard as a file URL",
        "Неизвестный режим файлового буфера Linux" => "Unknown Linux file clipboard mode",
        "Владелец системного буфера не ответил вовремя" => "System clipboard owner did not respond in time",
        "Системное перетаскивание требует главного потока" => "System drag requires the main thread",
        "Не удалось получить окно: {e}" => "Could not get window: {e}",
        "Окно не поддерживает AppKit" => "Window does not support AppKit",
        "Окно уже закрыто" => "Window is already closed",
        "Не удалось получить событие перетаскивания" => "Could not get drag event",
        "Перетаскивание отменено: кнопка мыши уже отпущена" => "Drag cancelled: mouse button already released",
        "Не удалось создать событие перетаскивания" => "Could not create drag event",
        "Не удалось прочитать путь файла" => "Could not read file path",
        "Не выбраны файлы для перетаскивания" => "No files selected for dragging",
        "Для переноса SFTP-файлов во внешнее приложение сначала скопируйте их в локальную папку" => "To drag SFTP files into an external application, copy them to a local folder first",
        "Не удалось передать {}: {e}" => "Could not transfer {}: {e}",
        "Недопустимый путь файла" => "Invalid file path",
        _ => russian,
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
    fn translations_cover_navigation_dialogs_and_errors_without_touching_unknown_text() {
        for (ru, en) in [
            ("Не удалось проверить источник", "Could not check source"),
            ("Не удалось скопировать", "Could not copy"),
            ("Настройки", "Settings"),
            ("Домашняя", "Home"),
            (
                "Удалить с сервера навсегда?",
                "Permanently delete from server?",
            ),
            (
                "Системный буфер изменился. Повторите вставку.",
                "System clipboard changed. Paste again.",
            ),
            ("СВЕДЕНИЯ", "DETAILS"),
        ] {
            assert_eq!(translate(true, ru), en);
            assert_eq!(translate(false, ru), ru);
        }
        assert_eq!(translate(true, "/tmp/Документы.txt"), "/tmp/Документы.txt");
    }
}
