use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub(super) fn detect() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let mut candidates = Vec::new();
        if let Some(developer) = std::env::var_os("DEVELOPER_DIR").map(PathBuf::from) {
            candidates.push(developer.join("usr/bin/opendiff"));
            candidates.push(developer.join("Contents/Developer/usr/bin/opendiff"));
        }
        candidates.push(PathBuf::from(
            "/Applications/Xcode.app/Contents/Developer/usr/bin/opendiff",
        ));
        if let Some(home) = dirs::home_dir() {
            candidates
                .push(home.join("Applications/Xcode.app/Contents/Developer/usr/bin/opendiff"));
        }
        // /usr/bin/opendiff can be present as a shim on Macs without Xcode.
        candidates
            .into_iter()
            .find(|path| path.is_absolute() && super::executable(path))
    }
    #[cfg(not(target_os = "macos"))]
    None
}

pub(crate) fn launch_diff(
    tool: &Path,
    files: &[PathBuf; 2],
    report: impl FnOnce(Result<(), String>),
) {
    let mut child = match spawn(tool, files) {
        Ok(child) => child,
        Err(error) => {
            report(Err(error));
            return;
        }
    };
    let deadline = Instant::now() + Duration::from_millis(700);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                report(if status.success() {
                    Ok(())
                } else {
                    Err(crate::tr_format!(
                        "Внешний инструмент завершился с ошибкой ({status})",
                        "The external tool exited with an error ({status})"
                    ))
                });
                return;
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(15)),
            Ok(None) => {
                // Some tools keep the process attached while their window is
                // open. Report startup, then reap it outside the UI thread.
                report(Ok(()));
                let _ = child.wait();
                return;
            }
            Err(error) => {
                report(Err(start_error(error)));
                let _ = child.wait();
                return;
            }
        }
    }
}

fn spawn(tool: &Path, files: &[PathBuf; 2]) -> Result<Child, String> {
    if !tool.is_absolute() || !super::executable(tool) {
        return Err(crate::i18n::text("Внешний инструмент сравнения недоступен").into());
    }
    for path in files {
        if !path.is_absolute() {
            return Err(crate::i18n::text("Внешнему инструменту нужны два локальных файла").into());
        }
        crate::fs_ops::open_regular_source(path).map_err(|error| {
            crate::tr_format!(
                "Не удалось открыть {}: {error}",
                "Could not open {}: {error}",
                path.display()
            )
        })?;
    }
    Command::new(tool)
        .args(files)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(start_error)
}

fn start_error(error: std::io::Error) -> String {
    crate::tr_format!(
        "Не удалось запустить внешний инструмент: {error}",
        "Could not launch the external tool: {error}"
    )
}
