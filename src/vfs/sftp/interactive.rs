use crate::i18n::text;
use ssh2::{KeyboardInteractivePrompt, Prompt, Session};
use std::{
    net::{Shutdown, TcpStream},
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

const PROMPT_TIMEOUT: Duration = Duration::from_secs(60);
const EXCHANGE_TIMEOUT_MS: u32 = 180_000;
const CANCEL_POLL: Duration = Duration::from_millis(50);

#[derive(Clone, Default)]
pub(crate) struct ConnectionControl(Arc<Mutex<Attempt>>);

enum Attempt {
    Pending(Option<TcpStream>),
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
}

impl Abort {
    fn message(self) -> &'static str {
        text(match self {
            Self::Cancelled => "Подключение отменено",
            Self::TimedOut => "Время интерактивной SSH-аутентификации истекло",
        })
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
        if let Some(socket) = socket {
            let _ = socket.shutdown(Shutdown::Both);
        }
        true
    }

    pub(crate) fn cancel(&self) -> bool {
        self.stop(Abort::Cancelled)
    }

    pub(crate) fn can_cancel(&self) -> bool {
        matches!(*self.0.lock().unwrap(), Attempt::Pending(_))
    }

    pub(super) fn check(&self) -> Result<(), String> {
        match *self.0.lock().unwrap() {
            Attempt::Aborted(reason) => Err(reason.message().into()),
            _ => Ok(()),
        }
    }

    pub(super) fn attach(&self, socket: &TcpStream) -> Result<(), String> {
        let copy = socket.try_clone().map_err(|error| error.to_string())?;
        let mut attempt = self.0.lock().unwrap();
        match &mut *attempt {
            Attempt::Pending(slot) => {
                *slot = Some(copy);
                Ok(())
            }
            Attempt::Aborted(reason) => Err(reason.message().into()),
            Attempt::Finalizing => Err("SSH connection already finalized".into()),
        }
    }

    /// Release the shutdown handle before the session can be shared.
    pub(super) fn finalize(&self) -> Result<(), String> {
        let mut attempt = self.0.lock().unwrap();
        if let Attempt::Aborted(reason) = *attempt {
            return Err(reason.message().into());
        }
        *attempt = Attempt::Finalizing;
        Ok(())
    }
}

type Answers = Vec<Zeroizing<String>>;

pub(crate) struct Question {
    pub label: String,
    pub echo: bool,
}

pub(crate) struct AuthRequest {
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
    requests: mpsc::Sender<AuthRequest>,
    notify: Box<dyn Fn() + Send>,
    timeout: Duration,
    exchange_deadline: Option<Instant>,
}

impl InteractiveAuth {
    pub(crate) fn new(notify: impl Fn() + Send + 'static) -> (Self, mpsc::Receiver<AuthRequest>) {
        let (requests, receive) = mpsc::channel();
        (
            Self {
                control: ConnectionControl::default(),
                requests,
                notify: Box::new(notify),
                timeout: PROMPT_TIMEOUT,
                exchange_deadline: None,
            },
            receive,
        )
    }

    pub(crate) fn control(&self) -> ConnectionControl {
        self.control.clone()
    }

    pub(super) fn authenticate(&mut self, session: &Session, user: &str) {
        let previous_timeout = session.timeout();
        let deadline = Instant::now() + Duration::from_millis(u64::from(EXCHANGE_TIMEOUT_MS));
        self.exchange_deadline = Some(deadline);
        // libssh2's blocking timeout includes every prompt in the exchange.
        session.set_timeout(EXCHANGE_TIMEOUT_MS);
        let result = session.userauth_keyboard_interactive(user, self);
        session.set_timeout(previous_timeout);
        self.exchange_deadline = None;
        if result.is_err() && Instant::now() >= deadline {
            self.control.stop(Abort::TimedOut);
        }
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
        if self.requests.send(request).is_err() {
            self.control.cancel();
            return None;
        }
        (self.notify)();
        if prompts.is_empty() {
            return Some(Vec::new());
        }
        loop {
            if self.control.check().is_err() {
                return None;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.control.stop(Abort::TimedOut);
                return None;
            }
            match receive.recv_timeout(remaining.min(CANCEL_POLL)) {
                Ok(answers) if answers.len() == prompts.len() => {
                    if Instant::now() >= deadline {
                        self.control.stop(Abort::TimedOut);
                        return None;
                    }
                    return self.control.check().ok().map(|()| answers);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                _ => {
                    self.control.cancel();
                    return None;
                }
            }
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
        let prompt = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(prompt.deadline, deadline);
        prompt.answer(vec![Zeroizing::new("fixture-answer".into())]);
        assert_eq!(worker.join().unwrap(), ["fixture-answer"]);
    }
}
