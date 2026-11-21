use crate::{
    file_error::PreviewError,
    operations::{Cancellation, TransferError},
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
}

impl<R> Pending<R> {
    pub(super) fn poll(&self) -> Option<Result<R, TransferError>> {
        match self.replies.try_recv() {
            Ok(reply) => Some(reply),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                Some(Err(PreviewError::WorkerUnavailable.into()))
            }
        }
    }
}

impl<R> Drop for Pending<R> {
    fn drop(&mut self) {
        self.cancel.request();
    }
}

pub(super) struct Worker<W, R>(mpsc::Sender<Job<W, R>>);

impl<W: Send + 'static, R: Send + 'static> Worker<W, R> {
    pub(super) fn start(
        name: &str,
        ctx: &egui::Context,
        run: impl Fn(&W, &Cancellation) -> Result<R, TransferError> + Send + 'static,
    ) -> Result<Self, TransferError> {
        let (requests, jobs) = mpsc::channel::<Job<W, R>>();
        let ctx = ctx.clone();
        let viewport = ctx.viewport_id();
        thread::Builder::new().name(name.into()).spawn(move || {
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
        })?;
        Ok(Self(requests))
    }

    pub(super) fn load(&self, work: W) -> Result<Pending<R>, TransferError> {
        let (reply, replies) = mpsc::channel();
        let cancel = Cancellation::default();
        self.0
            .send(Job {
                work,
                reply,
                cancel: cancel.clone(),
            })
            .map_err(|_| PreviewError::WorkerUnavailable)?;
        Ok(Pending { replies, cancel })
    }
}
