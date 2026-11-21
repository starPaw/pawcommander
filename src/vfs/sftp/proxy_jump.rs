use super::{
    at_target, authenticate, login_primary, open_verified, route_error, ssh_failure,
    validate_endpoint, verify_session, Endpoint, HostKeyRole, InteractiveAuth, PrimaryAuth,
    TrustFiles, IO_TIMEOUT_MS,
};
use crate::file_error::{ConnectionError, SshStage};
use crate::operations::TransferError;
use crate::vfs::JumpHost;
use ssh2::{BlockDirections, Channel, Session};
use std::{
    io::{self, ErrorKind, Read, Write},
    net::Shutdown,
    ops::Range,
    os::{fd::AsRawFd, unix::net::UnixStream},
    time::Duration,
};

pub(super) struct Login<'a> {
    pub use_saved: bool,
    pub passphrase: &'a str,
}

pub(super) fn open(
    target: &Endpoint,
    jump: &JumpHost,
    trust: &TrustFiles,
    mut auth: Option<&mut InteractiveAuth>,
    login: Login<'_>,
) -> Result<Session, TransferError> {
    validate_endpoint(target).map_err(|error| at_target(target, error))?;
    if let Some(auth) = auth.as_deref_mut() {
        auth.set_host_key_role(HostKeyRole::Jump);
    }
    let socket = tunnel(target, &jump.endpoint(), trust, auth.as_deref_mut(), login)
        .map_err(|error| route_error(jump.label(), true, error))?;
    let mut session = Session::new()
        .map_err(ssh_failure(SshStage::Session))
        .map_err(|error| at_target(target, error))?;
    session.set_tcp_stream(socket);
    if let Some(auth) = auth.as_deref_mut() {
        auth.set_host_key_role(HostKeyRole::Target);
    }
    verify_session(session, target, trust, auth.as_deref())
        .map_err(|error| at_target(target, error))
}

fn tunnel(
    target: &Endpoint,
    jump: &Endpoint,
    trust: &TrustFiles,
    auth: Option<&mut InteractiveAuth>,
    login: Login<'_>,
) -> Result<UnixStream, TransferError> {
    let control = auth.as_deref().map(InteractiveAuth::control);
    let session = open_verified(jump, trust, auth.as_deref())?;
    let saved = if login.use_saved && jump.key.is_some() && login.passphrase.is_empty() {
        crate::credentials::load(jump)?
    } else {
        None
    };
    let primary = PrimaryAuth::new(
        jump,
        "",
        saved
            .as_ref()
            .map_or(login.passphrase, |key| key.passphrase.as_str()),
        saved.as_ref().map(|key| key.private_key.as_str()),
    );
    if let Some(auth) = auth {
        authenticate(&session, jump, &primary, auth)?;
    } else {
        login_primary(&session, &jump.user, primary)?;
    }
    if let Some(control) = &control {
        control.check()?;
    }
    let channel = session
        .channel_direct_tcpip(target.host_name(), target.port, None)
        .map_err(|source| ConnectionError::Forward {
            target: target.label(),
            source,
        })?;
    let (socket, peer) = UnixStream::pair().map_err(ConnectionError::Transport)?;
    let timeout = Some(Duration::from_millis(u64::from(IO_TIMEOUT_MS)));
    socket
        .set_read_timeout(timeout)
        .map_err(ConnectionError::Transport)?;
    socket
        .set_write_timeout(timeout)
        .map_err(ConnectionError::Transport)?;
    peer.set_nonblocking(true)
        .map_err(ConnectionError::Transport)?;
    if let Some(control) = &control {
        let cancel_socket = socket.try_clone().map_err(ConnectionError::Transport)?;
        control.attach(move || {
            let _ = cancel_socket.shutdown(Shutdown::Both);
        })?;
    }
    session.set_blocking(false);
    std::thread::Builder::new()
        .name("pawcommander-ssh-jump".into())
        .spawn(move || {
            let _ = relay(peer, session, channel);
        })
        .map_err(ConnectionError::Transport)?;
    Ok(socket)
}

struct Buffer {
    bytes: [u8; 32 * 1024],
    pending: Range<usize>,
}

impl Buffer {
    fn new() -> Self {
        Self {
            bytes: [0; 32 * 1024],
            pending: 0..0,
        }
    }

    fn fill(&mut self, from: &mut impl Read) -> io::Result<usize> {
        let size = from.read(&mut self.bytes)?;
        self.pending = 0..size;
        Ok(size)
    }

    fn drain(&mut self, to: &mut impl Write) -> io::Result<usize> {
        let size = to.write(&self.bytes[self.pending.clone()])?;
        self.pending.start += size;
        Ok(size)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChannelOperation {
    Read,
    Write,
}

fn relay(mut peer: UnixStream, jump: Session, mut channel: Channel) -> io::Result<()> {
    let mut from_target = Buffer::new();
    let mut to_target = Buffer::new();
    let mut blocked = None;
    loop {
        let mut progressed = false;
        let mut peer_events = 0;
        let mut jump_events = 0;

        if from_target.pending.is_empty() {
            match from_target.fill(&mut peer) {
                Ok(0) => return Ok(()), // The target session closed its socket.
                Ok(_) => progressed = true,
                Err(error) if error.kind() == ErrorKind::WouldBlock => peer_events |= libc::POLLIN,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        if !to_target.pending.is_empty() {
            match to_target.drain(&mut peer) {
                Ok(0) => return Err(ErrorKind::WriteZero.into()),
                Ok(_) => progressed = true,
                Err(error) if error.kind() == ErrorKind::WouldBlock => peer_events |= libc::POLLOUT,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }

        if to_target.pending.is_empty() && blocked != Some(ChannelOperation::Write) {
            blocked = None;
            match to_target.fill(&mut channel) {
                Ok(0) if channel.eof() => return Ok(()),
                Ok(0) => jump_events |= libc::POLLIN,
                Ok(_) => progressed = true,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    let events = wait_events(&jump);
                    jump_events |= events;
                    if events & libc::POLLOUT != 0 {
                        blocked = Some(ChannelOperation::Read);
                    }
                }
                Err(error) => return Err(error),
            }
        }
        // libssh2 may have partially sent a packet. Retry that same operation
        // with the original bytes before another operation can send packets.
        if !from_target.pending.is_empty() && blocked != Some(ChannelOperation::Read) {
            blocked = None;
            match from_target.drain(&mut channel) {
                Ok(0) => jump_events |= libc::POLLIN,
                Ok(_) => progressed = true,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    let events = wait_events(&jump);
                    jump_events |= events;
                    if events & libc::POLLOUT != 0 {
                        blocked = Some(ChannelOperation::Write);
                    }
                }
                Err(error) => return Err(error),
            }
        }
        if !progressed && !wait(&peer, peer_events, &jump, jump_events)? {
            return Ok(());
        }
    }
}

fn wait_events(session: &Session) -> libc::c_short {
    match session.block_directions() {
        BlockDirections::None | BlockDirections::Inbound => libc::POLLIN,
        BlockDirections::Outbound => libc::POLLOUT,
        BlockDirections::Both => libc::POLLIN | libc::POLLOUT,
    }
}

fn wait(
    peer: &UnixStream,
    peer_events: libc::c_short,
    jump: &Session,
    jump_events: libc::c_short,
) -> io::Result<bool> {
    let mut fds = [
        libc::pollfd {
            fd: peer.as_raw_fd(),
            events: peer_events,
            revents: 0,
        },
        libc::pollfd {
            fd: if jump_events == 0 {
                -1
            } else {
                jump.as_raw_fd()
            },
            events: jump_events,
            revents: 0,
        },
    ];
    // SAFETY: Both descriptors are borrowed for this call. poll writes only to the array.
    let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != ErrorKind::Interrupted {
            return Err(error);
        }
    }
    Ok(fds[0].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) == 0)
}
