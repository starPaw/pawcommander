use super::{
    comparison::{ComparisonBody, ComparisonDialog, ComparisonResponse},
    export_file,
    file_dialog::{Mode, Picker},
    PawCommander,
};
use crate::{
    file_error::{ExportKind, ExportTarget},
    i18n::{display, text, Language},
    operations::TransferError,
    result_history::DiagnosticFailure,
    theme::paint_focus,
};
use eframe::egui;
use std::{borrow::Cow, path::PathBuf, process::Command, sync::mpsc, thread};

const RECENT_FAILURES: usize = 8;
const MAX_DETAIL_CHARS: usize = 512;
const MAX_DRAFT_CHARS: usize = 32 * 1024;

struct Snapshot {
    summary: String,
    details: Option<String>,
}

impl Snapshot {
    fn capture<'a>(
        language: Language,
        scale: u16,
        loaded: bool,
        history_error: Option<&str>,
        current_error: Option<&str>,
        failures: impl Iterator<Item = DiagnosticFailure<'a>>,
    ) -> Self {
        let mut details = String::new();
        if let Some(message) = current_error {
            detail_line(&mut details, text!("Текущее сообщение об ошибке"), message);
        }
        if let Some(message) = history_error {
            detail_line(&mut details, text!("Ошибка журнала результатов"), message);
        }
        let mut recent = 0;
        for failure in failures.take(RECENT_FAILURES) {
            recent += 1;
            let time = i64::try_from(failure.finished)
                .ok()
                .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
                .map(|date| date.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                .unwrap_or_else(|| text!("Нет данных").into());
            details.push('\n');
            detail_line(&mut details, &time, failure.label);
            detail_line(&mut details, text!("Результат операции"), failure.message);
            if let Some(reason) = failure.reason.filter(|reason| *reason != failure.message) {
                detail_line(&mut details, text!("Причина ошибки"), reason);
            }
        }
        let language = match language {
            Language::English => "en",
            Language::Russian => "ru",
            Language::Czech => "cs",
        };
        let history = if history_error.is_some() {
            text!("Ошибка журнала результатов")
        } else if loaded {
            text!("Журнал загружен")
        } else {
            text!("Журнал ещё загружается")
        };
        let current = if current_error.is_some() {
            text!("Есть сообщение")
        } else {
            text!("Нет сообщения")
        };
        let summary = [
            (text!("Версия приложения"), env!("CARGO_PKG_VERSION").into()),
            (
                text!("Архитектура приложения"),
                std::env::consts::ARCH.into(),
            ),
            (text!("Язык"), language.into()),
            (text!("Масштаб интерфейса"), format!("{scale}%")),
            (text!("Журнал результатов"), history.into()),
            (
                text!("Недавние ошибки операций (до 8)"),
                display::integer(recent),
            ),
            (text!("Текущее сообщение об ошибке"), current.into()),
        ]
        .into_iter()
        .map(|(label, value)| format!("{label} = {value}\n"))
        .collect();
        Self {
            summary,
            details: (!details.is_empty())
                .then(|| format!("\n{}\n{details}", text!("Сведения об ошибках"))),
        }
    }
}

fn detail_line(output: &mut String, label: &str, value: &str) {
    output.push_str(&bounded_detail(label));
    output.push_str(" = ");
    output.push_str(&bounded_detail(value));
    output.push('\n');
}

fn bounded_detail(value: &str) -> String {
    let mut chars = value.chars();
    let line: String = chars
        .by_ref()
        .take(MAX_DETAIL_CHARS)
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let mut line = line.escape_debug().to_string();
    if chars.next().is_some() {
        line.push_str("...");
    }
    line
}

fn os_release(raw: &str) -> Option<String> {
    let mut product = None;
    let mut build = None;
    for (key, value) in raw.lines().filter_map(|line| line.split_once(':')) {
        let value = value.trim();
        if value.len() > 32 || value.is_empty() {
            continue;
        }
        match key {
            "ProductVersion" if value.bytes().all(|ch| ch.is_ascii_digit() || ch == b'.') => {
                product = Some(value);
            }
            "BuildVersion" if value.bytes().all(|ch| ch.is_ascii_alphanumeric()) => {
                build = Some(value);
            }
            _ => {}
        }
    }
    let version = product?;
    Some(match build {
        Some(build) => format!("{version} ({build})"),
        None => version.into(),
    })
}

fn collect_macos() -> String {
    Command::new("/usr/bin/sw_vers")
        .output()
        .ok()
        .filter(|result| result.status.success())
        .and_then(|result| os_release(&String::from_utf8_lossy(&result.stdout)))
        .unwrap_or_else(|| text!("Нет данных").into())
}

pub(super) struct DiagnosticDialog {
    draft: String,
    details: Option<String>,
    system_summary: Option<(mpsc::Receiver<String>, String)>,
    picker: Option<Picker>,
    destination: Option<PathBuf>,
    export: Option<(ExportTarget, mpsc::Receiver<Result<(), TransferError>>)>,
    status: Option<Status>,
}

enum Status {
    PickerError(String),
    Export(Result<(), TransferError>),
}

impl DiagnosticDialog {
    fn new(snapshot: Snapshot, ctx: &egui::Context) -> Self {
        let (send, receive) = mpsc::channel();
        let ctx = ctx.clone();
        let collecting = thread::Builder::new()
            .name("pawcommander-diagnostics".into())
            .spawn(move || {
                let _ = send.send(collect_macos());
                ctx.request_repaint_of(egui::ViewportId::ROOT);
            });
        let draft = initial_draft(text!("Нет данных"), &snapshot.summary);
        Self {
            draft,
            details: snapshot.details,
            system_summary: collecting.ok().map(|_| (receive, snapshot.summary)),
            picker: None,
            destination: None,
            export: None,
            status: None,
        }
    }

    pub(super) fn poll(&mut self) {
        if let Some((receive, summary)) = &self.system_summary {
            match receive.try_recv() {
                Ok(macos) => {
                    self.draft = initial_draft(&macos, summary);
                    self.system_summary = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.system_summary = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(result) = self.picker.as_ref().and_then(Picker::poll) {
            self.picker = None;
            match result {
                Ok(path) => self.destination = path,
                Err(error) => self.status = Some(Status::PickerError(error)),
            }
        }
        if let Some((target, receive)) = &self.export {
            let result = match receive.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Disconnected) => Err(target.worker_stopped()),
                Err(mpsc::TryRecvError::Empty) => return,
            };
            self.export = None;
            self.status = Some(Status::Export(result));
        }
    }

    pub(super) fn show(&mut self, ctx: &egui::Context) -> ComparisonResponse {
        if let Some(path) = self.destination.take() {
            let target = ExportTarget {
                kind: ExportKind::DiagnosticReport,
                path,
            };
            let (send, receive) = mpsc::channel();
            self.export = Some((target.clone(), receive));
            self.status = None;
            return ComparisonResponse::ExportDiagnostic(target, self.draft.clone(), send);
        }
        let mut add_details = false;
        let mut save = false;
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::unique("diagnostic-report")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 44.0).clamp(240.0, 660.0));
            ui.heading(text!("Диагностический отчёт"));
            if let Some(status) = &self.status {
                let (message, failed): (Cow<'_, str>, bool) = match status {
                    Status::PickerError(error) => (error.as_str().into(), true),
                    Status::Export(Ok(())) => {
                        (text!("Диагностический отчёт сохранён.").into(), false)
                    }
                    Status::Export(Err(error)) => (
                        crate::i18n::file_error(error).into(),
                        !matches!(error, TransferError::Cancelled),
                    ),
                };
                let color = if failed {
                    ui.visuals().error_fg_color
                } else {
                    ui.visuals().text_color()
                };
                ui.colored_label(color, message);
            }
            let editable = self.picker.is_none() && self.export.is_none();
            if self.system_summary.is_some() {
                ui.label(text!("Подготовка отчёта…"));
            } else {
                let caption = ui.label(text!(
                    "Проверьте текст и удалите личные данные перед сохранением."
                ));
                let height = (ctx.content_rect().height() - 210.0).max(85.0);
                egui::ScrollArea::vertical()
                    .max_height(height)
                    .show(ui, |ui| {
                        ui.add_enabled(
                            editable,
                            egui::TextEdit::multiline(&mut self.draft)
                                .font(egui::TextStyle::Monospace)
                                .desired_width(f32::INFINITY)
                                .desired_rows(13)
                                .char_limit(MAX_DRAFT_CHARS),
                        )
                        .labelled_by(caption.id);
                    });
            }
            if self.export.is_some() {
                ui.label(text!("Сохранение отчёта…"));
            }
            ui.horizontal_wrapped(|ui| {
                let ready = editable && self.system_summary.is_none();
                if let Some(details) = &self.details {
                    let fits =
                        self.draft.chars().count() + details.chars().count() <= MAX_DRAFT_CHARS;
                    add_details = button(ui, ready && fits, text!("Добавить сведения об ошибках"));
                }
                save = button(
                    ui,
                    ready && !self.draft.trim().is_empty(),
                    text!("Сохранить отчёт…"),
                );
                close = button(ui, self.picker.is_none(), text!("Закрыть"));
            });
        });
        if add_details && let Some(details) = self.details.take() {
            self.draft.push_str(&details);
        }
        if save {
            match Picker::open(
                Mode::Save {
                    suggested_name: "PawCommander-diagnostics.txt",
                    message: text!("Выберите имя файла для отчёта."),
                },
                ctx,
            ) {
                Ok(picker) => self.picker = Some(picker),
                Err(error) => self.status = Some(Status::PickerError(error)),
            }
        }
        if self.picker.is_none() && (close || modal.should_close()) {
            ComparisonResponse::Close
        } else {
            ComparisonResponse::Open
        }
    }
}

fn initial_draft(macos: &str, summary: &str) -> String {
    let title = text!("Диагностический отчёт PawCommander");
    format!("{title}\n\nmacOS = {macos}\n{summary}")
}

fn button(ui: &mut egui::Ui, enabled: bool, label: &str) -> bool {
    let response = ui.add_enabled(enabled, egui::Button::new(label));
    paint_focus(ui, &response, 4);
    response.clicked()
}

impl PawCommander {
    pub(super) fn open_diagnostics(&mut self, focus: egui::Id, ctx: &egui::Context) {
        let history_error = self
            .history
            .error
            .as_deref()
            .map(crate::i18n::history_error);
        let snapshot = Snapshot::capture(
            self.settings.language,
            self.settings.ui_scale,
            self.history.diagnostics_loaded(),
            history_error.as_deref(),
            self.notice
                .as_ref()
                .and_then(|(message, failed)| failed.then_some(message.as_str())),
            self.history.diagnostic_failures(),
        );
        self.comparison = Some(ComparisonDialog::new(
            ComparisonBody::Diagnostics(Box::new(DiagnosticDialog::new(snapshot, ctx))),
            Some(focus),
            ctx,
        ));
    }

    pub(super) fn export_diagnostics(
        &mut self,
        target: ExportTarget,
        draft: String,
        send: mpsc::Sender<Result<(), TransferError>>,
        ctx: &egui::Context,
    ) {
        self.start_export(ctx, target, Some(send), move |target| {
            export_file::write_new(target, draft.as_bytes())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_error::{ExportIoAction, ExportProblem, Failure};

    #[test]
    fn report_summary_keeps_raw_errors_for_explicit_inclusion_and_bounds_them() {
        let details = "PRIVATE_CAUSE".repeat(MAX_DETAIL_CHARS);
        let records = (0..20).map(|_| DiagnosticFailure {
            finished: 0,
            label: "PRIVATE_TASK",
            message: "PRIVATE_RESULT",
            reason: Some(&details),
        });
        let snapshot = Snapshot::capture(
            Language::English,
            125,
            true,
            Some("/Users/PRIVATE_HISTORY"),
            Some("PRIVATE_NOTICE\u{1b}"),
            records,
        );
        let draft = initial_draft("27.0.1 (26A434)", &snapshot.summary);
        assert!(draft.contains(env!("CARGO_PKG_VERSION")));
        assert!(draft.contains("27.0.1 (26A434)") && draft.contains("125%"));
        assert!(draft.lines().any(|line| line.ends_with(" = 8")));
        assert!(!draft.contains("PRIVATE_"));
        let included = snapshot.details.unwrap();
        assert!(included.contains("PRIVATE_NOTICE") && included.contains("PRIVATE_HISTORY"));
        assert_eq!(included.matches("PRIVATE_TASK").count(), RECENT_FAILURES);
        assert!(!included.contains("\u{1b}") && !included.contains(&details));
    }

    #[test]
    fn macos_release_reads_only_version_and_build_fields() {
        let raw = "ProductVersion:\t27.0.1\nBuildVersion:\t26A434\nName: Private user\n";
        assert_eq!(os_release(raw).as_deref(), Some("27.0.1 (26A434)"));
        assert_eq!(os_release("ProductVersion: invalid"), None);
        assert_eq!(
            os_release("ProductVersion: 14.5\nBuildVersion: /Users/path").as_deref(),
            Some("14.5")
        );
    }

    #[test]
    fn report_writes_exact_draft_and_keeps_it_when_export_fails() {
        let draft = "Summary\nEdited message = [redacted]\n";
        let folder = tempfile::tempdir().unwrap();
        let target = ExportTarget {
            kind: ExportKind::DiagnosticReport,
            path: folder.path().join("report.txt"),
        };
        export_file::write_new(&target, draft.as_bytes()).unwrap();
        let failure = export_file::write_new(&target, b"other content").unwrap_err();
        let (send, receive) = mpsc::channel();
        send.send(Err(failure)).unwrap();
        let mut dialog = DiagnosticDialog {
            draft: draft.into(),
            details: None,
            system_summary: None,
            picker: None,
            destination: None,
            export: Some((target.clone(), receive)),
            status: None,
        };
        dialog.poll();
        assert_eq!(dialog.draft, draft);
        assert!(dialog.export.is_none());
        let Some(Status::Export(Err(TransferError::Failed(Failure::Export(error))))) =
            dialog.status
        else {
            panic!("expected the original typed error in the report dialog");
        };
        assert_eq!(error.target.path, target.path);
        assert!(matches!(error.problem, ExportProblem::Io {
            action: ExportIoAction::Publish, source,
        } if source.raw_os_error() == Some(libc::EEXIST)));
        assert_eq!(std::fs::read(&target.path).unwrap(), draft.as_bytes());
        assert_eq!(folder.path().read_dir().unwrap().count(), 1);
    }
}
