use std::{
    process::Child,
    thread,
    time::{Duration, Instant},
};

pub(super) fn report_startup(mut child: Child, report: impl FnOnce(Result<(), String>)) {
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
                // Windowed tools may stay attached. Report startup and reap them on this worker.
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

pub(super) fn start_error(error: std::io::Error) -> String {
    crate::tr_format!(
        "Не удалось запустить внешний инструмент: {error}",
        "Could not launch the external tool: {error}"
    )
}
