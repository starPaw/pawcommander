use crate::operations::Cancellation;
use eframe::egui;
use std::{io, sync::mpsc, thread};

struct Job<W, R> {
    work: W,
    reply: mpsc::Sender<Result<R, String>>,
    cancel: Cancellation,
}

pub(super) struct Pending<R> {
    replies: mpsc::Receiver<Result<R, String>>,
    cancel: Cancellation,
}

impl<R> Pending<R> {
    pub(super) fn poll(&self) -> Option<Result<R, String>> {
        match self.replies.try_recv() {
            Ok(reply) => Some(reply),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(unavailable())),
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
        run: impl Fn(&W, &Cancellation) -> Result<R, String> + Send + 'static,
    ) -> io::Result<Self> {
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

    pub(super) fn load(&self, work: W) -> Result<Pending<R>, String> {
        let (reply, replies) = mpsc::channel();
        let cancel = Cancellation::default();
        self.0
            .send(Job {
                work,
                reply,
                cancel: cancel.clone(),
            })
            .map_err(|_| unavailable())?;
        Ok(Pending { replies, cancel })
    }
}

fn unavailable() -> String {
    crate::i18n::text!("Служба предпросмотра недоступна").into()
}
