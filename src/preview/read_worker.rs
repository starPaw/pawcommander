use crate::{
    file_error::{PreviewWorkerError, PreviewWorkerKind},
    operations::{Cancellation, TransferError},
    vfs::Location,
};
use eframe::egui;
use std::{sync::mpsc, thread};

struct Job<W, R> {
    work: W,
    reply: mpsc::Sender<Result<R, TransferError>>,
    cancel: Cancellation,
}

pub(super) struct Pending<R> {
    replies: mpsc::Receiver<Result<R, TransferError>>,
    cancel: Cancellation,
    location: Location,
    kind: PreviewWorkerKind,
}

impl<R> Pending<R> {
    pub(super) fn poll(&self) -> Option<Result<R, TransferError>> {
        if self.cancel.is_requested() {
            return Some(Err(TransferError::Cancelled));
        }
        match self.replies.try_recv() {
            Ok(reply) => Some(reply),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(PreviewWorkerError::Disconnected
                .at(&self.location, self.kind)
                .into())),
        }
    }
}

impl<R> Drop for Pending<R> {
    fn drop(&mut self) {
        self.cancel.request();
    }
}

pub(super) struct Worker<W, R> {
    requests: mpsc::Sender<Job<W, R>>,
    location: Location,
    kind: PreviewWorkerKind,
}

impl<W: Send + 'static, R: Send + 'static> Worker<W, R> {
    pub(super) fn start(
        name: &str,
        location: Location,
        kind: PreviewWorkerKind,
        ctx: &egui::Context,
        run: impl Fn(&W, &Cancellation) -> Result<R, TransferError> + Send + 'static,
    ) -> Result<Self, TransferError> {
        let (requests, jobs) = mpsc::channel::<Job<W, R>>();
        let ctx = ctx.clone();
        let viewport = ctx.viewport_id();
        thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                while let Ok(mut job) = jobs.recv() {
                    while let Ok(newer) = jobs.try_recv() {
                        job = newer;
                    }
                    if job.cancel.is_requested() {
                        continue;
                    }
                    let reply = run(&job.work, &job.cancel);
                    if !job.cancel.is_requested() && job.reply.send(reply).is_ok() {
                        ctx.request_repaint_of(viewport);
                    }
                }
            })
            .map_err(|source| PreviewWorkerError::Start(source).at(&location, kind))?;
        Ok(Self {
            requests,
            location,
            kind,
        })
    }

    pub(super) fn load(&self, work: W) -> Result<Pending<R>, TransferError> {
        let (reply, replies) = mpsc::channel();
        let cancel = Cancellation::default();
        self.requests
            .send(Job {
                work,
                reply,
                cancel: cancel.clone(),
            })
            .map_err(|_| PreviewWorkerError::Disconnected.at(&self.location, self.kind))?;
        Ok(Pending {
            replies,
            cancel,
            location: self.location.clone(),
            kind: self.kind,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_error::{Failure, PreviewError};

    #[test]
    fn pending_reader_reports_its_location_and_distinguishes_cancellation() {
        let location = Location::Local("/fixtures/read.bin".into());
        let (reply, replies) = mpsc::channel::<Result<(), TransferError>>();
        let pending = Pending {
            replies,
            cancel: Cancellation::default(),
            location: location.clone(),
            kind: PreviewWorkerKind::Hex,
        };
        assert!(pending.poll().is_none());
        drop(reply);
        let Some(Err(TransferError::Failed(Failure::Preview(error)))) = pending.poll() else {
            panic!("expected the active reader disconnect");
        };
        assert!(matches!(
            *error,
            PreviewError::Worker {
                location: target,
                kind: PreviewWorkerKind::Hex,
                cause: PreviewWorkerError::Disconnected,
            } if target == location
        ));
        pending.cancel.request();
        assert!(matches!(
            pending.poll(),
            Some(Err(TransferError::Cancelled))
        ));
    }
}
