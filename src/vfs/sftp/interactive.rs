use super::{host_key::HostKey, Endpoint};
use crate::file_error::ConnectionError;
use crate::i18n::text;
use crate::operations::TransferError;
use ssh2::{KeyboardInteractivePrompt, Prompt, Session};
use std::{
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

const PROMPT_TIMEOUT: Duration = Duration::from_secs(60);
const HOST_KEY_TIMEOUT: Duration = Duration::from_secs(120);
const EXCHANGE_TIMEOUT_MS: u32 = 180_000;
const CANCEL_POLL: Duration = Duration::from_millis(50);

#[derive(Clone, Default)]
pub(crate) struct ConnectionControl(Arc<Mutex<Attempt>>);

enum Attempt {
    Pending(Option<Box<dyn FnOnce() + Send>>),
    Aborted(Abort),
    Finalizing,
}

impl Default for Attempt {
    fn default() -> Self {
        Self::Pending(None)
    }
}

#[derive(Clone, Copy)]
enum Abort {
    Cancelled,
    TimedOut,
    HostKeyTimedOut,
}

impl Abort {
    fn error(self) -> TransferError {
        match self {
            Self::Cancelled => TransferError::Cancelled,
            Self::TimedOut => ConnectionError::InteractiveTimeout.into(),
            Self::HostKeyTimedOut => ConnectionError::HostKeyTimeout.into(),
        }
    }
}

impl ConnectionControl {
    /// The UI and the callback can end I/O without taking libssh2's session lock.
    fn stop(&self, reason: Abort) -> bool {
        let socket = {
            let mut attempt = self.0.lock().unwrap();
            let Attempt::Pending(socket) = &mut *attempt else {
                return false;
            };
            let socket = socket.take();
            *attempt = Attempt::Aborted(reason);
            socket
        };
        if let Some(shutdown) = socket {
            shutdown();
        }
        true
    }

    pub(crate) fn cancel(&self) -> bool {
        self.stop(Abort::Cancelled)
    }

    pub(crate) fn can_cancel(&self) -> bool {
        matches!(*self.0.lock().unwrap(), Attempt::Pending(_))
    }

    pub(super) fn check(&self) -> Result<(), TransferError> {
        match *self.0.lock().unwrap() {
            Attempt::Aborted(reason) => Err(reason.error()),
            _ => Ok(()),
        }
    }

    pub(super) fn attach(
        &self,
        shutdown: impl FnOnce() + Send + 'static,
    ) -> Result<(), TransferError> {
        let mut attempt = self.0.lock().unwrap();
        match &mut *attempt {
            Attempt::Pending(slot) => {
                *slot = Some(Box::new(shutdown));
                Ok(())
            }
            Attempt::Aborted(reason) => Err(reason.error()),
            Attempt::Finalizing => Err(ConnectionError::Finalized.into()),
        }
    }

    /// Release the shutdown handle before the session can be shared.
    pub(super) fn finalize(&self) -> Result<(), TransferError> {
        let mut attempt = self.0.lock().unwrap();
        if let Attempt::Aborted(reason) = *attempt {
            return Err(reason.error());
        }
        *attempt = Attempt::Finalizing;
        Ok(())
    }
}

type Answers = Vec<Zeroizing<String>>;

pub(crate) enum ConnectionPrompt {
    HostKey(HostKeyRequest),
    Authentication(AuthRequest),
}

pub(crate) struct HostKeyRequest {
    pub key: HostKey,
    pub title: &'static str,
    pub deadline: Instant,
    reply: mpsc::Sender<()>,
}

impl HostKeyRequest {
    pub(crate) fn approve(self) {
        let _ = self.reply.send(());
    }
}

pub(crate) struct Question {
    pub label: String,
    pub echo: bool,
}

pub(crate) struct AuthRequest {
    pub server: String,
    pub title: String,
    pub instructions: String,
    pub questions: Vec<Question>,
    pub deadline: Instant,
    reply: Option<mpsc::Sender<Answers>>,
}

impl AuthRequest {
    pub(crate) fn answer(self, answers: Answers) {
        if let Some(reply) = self.reply {
            let _ = reply.send(answers);
        }
    }
}

pub(crate) struct InteractiveAuth {
    control: ConnectionControl,
    requests: mpsc::Sender<ConnectionPrompt>,
    notify: Box<dyn Fn() + Send>,
    timeout: Duration,
    exchange_deadline: Option<Instant>,
    server: String,
    #[cfg(target_os = "macos")]
    host_key_title: Option<crate::i18n::MessageKey>,
}

impl InteractiveAuth {
    pub(crate) fn new(
        notify: impl Fn() + Send + 'static,
    ) -> (Self, mpsc::Receiver<ConnectionPrompt>) {
        let (requests, receive) = mpsc::channel();
        (
            Self {
                control: ConnectionControl::default(),
                requests,
                notify: Box::new(notify),
                timeout: PROMPT_TIMEOUT,
                exchange_deadline: None,
                server: String::new(),
                #[cfg(target_os = "macos")]
                host_key_title: None,
            },
            receive,
        )
    }

    pub(crate) fn control(&self) -> ConnectionControl {
        self.control.clone()
    }

    #[cfg(target_os = "macos")]
    pub(super) fn set_host_key_title(&mut self, title: crate::i18n::MessageKey) {
        self.host_key_title = Some(title);
    }

    pub(crate) fn confirm_host_key(&self, key: HostKey) -> Result<(), TransferError> {
        let (reply, receive) = mpsc::channel();
        let deadline = Instant::now() + HOST_KEY_TIMEOUT;
        let title = crate::i18n::key!("Проверка ключа сервера");
        #[cfg(target_os = "macos")]
        let title = self.host_key_title.unwrap_or(title);
        self.send(ConnectionPrompt::HostKey(HostKeyRequest {
            key,
            title: text(title),
            deadline,
            reply,
        }));
        self.wait(receive, deadline, Abort::HostKeyTimedOut)
    }

    pub(super) fn authenticate(
        &mut self,
        session: &Session,
        endpoint: &Endpoint,
    ) -> Result<(), ssh2::Error> {
        let previous_timeout = session.timeout();
        let deadline = Instant::now() + Duration::from_millis(u64::from(EXCHANGE_TIMEOUT_MS));
        self.exchange_deadline = Some(deadline);
        self.server = endpoint.label();
        // libssh2's blocking timeout includes every prompt in the exchange.
        session.set_timeout(EXCHANGE_TIMEOUT_MS);
        let result = session.userauth_keyboard_interactive(&endpoint.user, self);
        session.set_timeout(previous_timeout);
        self.exchange_deadline = None;
        if result.is_err() && Instant::now() >= deadline {
            self.control.stop(Abort::TimedOut);
        }
        result
    }

    #[cfg(test)]
    pub(crate) fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    fn request(&self, title: &str, instructions: &str, prompts: &[Prompt<'_>]) -> Option<Answers> {
        let (send, receive) = mpsc::channel();
        let round_deadline = Instant::now() + self.timeout;
        let deadline = self
            .exchange_deadline
            .map_or(round_deadline, |limit| limit.min(round_deadline));
        let request = AuthRequest {
            server: self.server.clone(),
            title: server_text(title),
            instructions: server_text(instructions),
            questions: prompts
                .iter()
                .map(|prompt| Question {
                    label: server_text(&prompt.text),
                    echo: prompt.echo,
                })
                .collect(),
            deadline,
            reply: (!prompts.is_empty()).then_some(send),
        };
        self.send(ConnectionPrompt::Authentication(request));
        if prompts.is_empty() {
            return self.control.check().ok().map(|()| Vec::new());
        }
        let answers = self.wait(receive, deadline, Abort::TimedOut).ok()?;
        if answers.len() == prompts.len() {
            Some(answers)
        } else {
            self.control.cancel();
            None
        }
    }

    fn send(&self, request: ConnectionPrompt) {
        if self.requests.send(request).is_ok() {
            (self.notify)();
        } else {
            self.control.cancel();
        }
    }

    fn wait<T>(
        &self,
        receive: mpsc::Receiver<T>,
        deadline: Instant,
        on_timeout: Abort,
    ) -> Result<T, TransferError> {
        loop {
            self.control.check()?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            let abort = match receive.recv_timeout(remaining.min(CANCEL_POLL)) {
                Ok(reply) if Instant::now() < deadline => {
                    return self.control.check().map(|()| reply);
                }
                Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => Abort::Cancelled,
                _ => on_timeout,
            };
            self.control.stop(abort);
            self.control.check()?;
            return Err(abort.error());
        }
    }
}

impl KeyboardInteractivePrompt for InteractiveAuth {
    fn prompt(&mut self, title: &str, instructions: &str, prompts: &[Prompt<'_>]) -> Vec<String> {
        self.request(title, instructions, prompts)
            .unwrap_or_default()
            .into_iter()
            // ssh2 copies each answer into its libssh2 response buffer.
            .map(|mut answer| std::mem::take(&mut *answer))
            .collect()
    }
}

fn server_text(value: &str) -> String {
    value
        .chars()
        .filter(|&ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_prompt_deadline_respects_the_remaining_exchange_time() {
        let (mut auth, requests) = InteractiveAuth::new(|| {});
        let deadline = Instant::now() + Duration::from_secs(30);
        auth.exchange_deadline = Some(deadline);
        let worker = std::thread::spawn(move || {
            auth.prompt(
                "Sign-in",
                "",
                &[Prompt {
                    text: "Code".into(),
                    echo: false,
                }],
            )
        });
        let ConnectionPrompt::Authentication(prompt) =
            requests.recv_timeout(Duration::from_secs(5)).unwrap()
        else {
            panic!("Expected an authentication prompt");
        };
        assert_eq!(prompt.deadline, deadline);
        prompt.answer(vec![Zeroizing::new("fixture-answer".into())]);
        assert_eq!(worker.join().unwrap(), ["fixture-answer"]);
    }
}
