use super::{
    frame_size, media_extension, AudioMetadata, Command, Metadata, Mode, OpenSource, Reply, State,
    VideoMetadata, VIDEO_EDGE,
};
use crate::{
    file_error::MediaNativeError,
    preview::{
        helper::{read_message, read_size, write_message},
        metadata::positive,
    },
};
use std::{
    ffi::{c_char, c_void, CStr, CString, OsStr},
    io::{self, BufReader, BufWriter, Read, Write},
    ptr::NonNull,
    sync::mpsc,
    thread,
    time::Duration,
};

#[repr(C)]
#[derive(Default)]
struct NativeState {
    position: f64,
    duration: f64,
    mode: i32,
    seeking: bool,
}

#[repr(C)]
#[derive(Default)]
struct NativeMetadata {
    fps: f64,
    sample_rate: f64,
    width: u32,
    height: u32,
    video_codec: u32,
    audio_codec: u32,
    channels: u32,
    has_video: bool,
    has_audio: bool,
}

#[repr(C)]
struct NativeError {
    code: i64,
    domain: [u8; 128],
}

impl NativeError {
    fn into_error(self) -> Option<MediaNativeError> {
        let domain = CStr::from_bytes_until_nul(&self.domain)
            .ok()?
            .to_str()
            .ok()?;
        (!domain.is_empty()).then(|| MediaNativeError {
            domain: domain.to_owned(),
            code: self.code,
        })
    }
}

unsafe extern "C" {
    fn paw_media_open(path: *const c_char) -> *mut c_void;
    fn paw_media_open_descriptor(fd: i32, extension: *const c_char, error: *mut i32)
        -> *mut c_void;
    fn paw_media_close(player: *mut c_void);
    fn paw_media_pump();
    fn paw_media_command(player: *mut c_void, command: i32, seconds: f64) -> bool;
    fn paw_media_state(player: *mut c_void, state: *mut NativeState) -> bool;
    fn paw_media_read_failure(player: *mut c_void) -> i32;
    fn paw_media_failure(player: *mut c_void, errors: *mut NativeError);
    fn paw_media_metadata(player: *mut c_void, metadata: *mut NativeMetadata);
    fn paw_media_frame(
        player: *mut c_void,
        rgba: *mut u8,
        capacity: usize,
        edge: u32,
        width: *mut u32,
        height: *mut u32,
    ) -> i32;
}

pub(crate) fn run() -> i32 {
    let mut output = BufWriter::new(io::stdout().lock());
    i32::from(serve(BufReader::new(io::stdin()), &mut output).is_err())
}

fn serve(mut input: impl Read + Send + 'static, output: &mut impl Write) -> io::Result<()> {
    let player = match open(&mut input)? {
        Ok(player) => player,
        Err(reply) => return fail(output, reply),
    };
    let (sender, commands) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("media-helper-input".into())
        .spawn(move || {
            while let Ok(command) = read_message(&mut input) {
                if sender.send(command).is_err() {
                    return;
                }
            }
        })?;
    write_message(output, &Reply::Opened)?;
    output.flush()?;
    let mut rgba = vec![0; frame_size([VIDEO_EDGE, VIDEO_EDGE]).unwrap()];
    let mut sent_metadata = false;
    loop {
        // AVPlayer is owned by this process's main thread. Give AVFoundation
        // time to publish loading, buffering and seek completions even when
        // the parent has no commands.
        unsafe { paw_media_pump() };
        let command = match commands.recv_timeout(Duration::from_millis(12)) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        };
        let state = player.apply(command);
        if let Some(reply) = player.read_failure() {
            return fail(output, reply);
        }
        let Some(state) = state else {
            return fail(output, player.failure());
        };
        let Ok(frame) = player.frame(&mut rgba, state.mode) else {
            return fail(output, player.failure());
        };
        let metadata = if !sent_metadata && state.mode != Mode::Loading {
            sent_metadata = true;
            Some(player.metadata())
        } else {
            None
        };
        write_message(
            output,
            &Reply::Snapshot {
                state,
                size: frame.map(|(size, _)| size),
                metadata,
            },
        )?;
        if let Some((_, len)) = frame {
            output.write_all(&rgba[..len])?;
        }
        output.flush()?;
    }
}

fn open(input: &mut impl Read) -> io::Result<Result<Player, Reply>> {
    let mut error = 0;
    let handle = match read_message(input)? {
        OpenSource::Path => {
            let size = read_size(input)?;
            if !(1..=65536).contains(&size) {
                return Ok(Err(Reply::Failed(Vec::new())));
            }
            let mut bytes = vec![0; size];
            input.read_exact(&mut bytes)?;
            let Ok(path) = CString::new(bytes) else {
                return Ok(Err(Reply::Failed(Vec::new())));
            };
            // The bridge uses the absolute filesystem representation during this call.
            unsafe { paw_media_open(path.as_ptr()) }
        }
        OpenSource::Descriptor { fd, extension } => {
            if fd <= libc::STDERR_FILENO {
                return Ok(Err(Reply::Failed(Vec::new())));
            }
            let Some(extension) = media_extension(OsStr::new(&extension)) else {
                return Ok(Err(Reply::Failed(Vec::new())));
            };
            let extension = CString::new(extension).expect("known media extension");
            // The inherited descriptor stays open for this dedicated process.
            unsafe { paw_media_open_descriptor(fd, extension.as_ptr(), &mut error) }
        }
    };
    let failure = if error == 0 {
        Reply::Failed(Vec::new())
    } else {
        Reply::ReadError(error)
    };
    Ok(NonNull::new(handle).map(Player).ok_or(failure))
}

fn fail(output: &mut impl Write, reply: Reply) -> io::Result<()> {
    write_message(output, &reply)?;
    output.flush()
}

struct Player(NonNull<c_void>);

impl Player {
    fn read_failure(&self) -> Option<Reply> {
        match unsafe { paw_media_read_failure(self.0.as_ptr()) } {
            0 => None,
            -1 => Some(Reply::Truncated),
            errno => Some(Reply::ReadError(errno)),
        }
    }

    fn failure(&self) -> Reply {
        let mut errors: [NativeError; 2] = std::array::from_fn(|_| NativeError {
            code: 0,
            domain: [0; 128],
        });
        // The bridge copies at most the primary error and its immediate cause.
        unsafe { paw_media_failure(self.0.as_ptr(), errors.as_mut_ptr()) };
        self.read_failure().unwrap_or_else(|| {
            Reply::Failed(
                errors
                    .into_iter()
                    .filter_map(NativeError::into_error)
                    .collect(),
            )
        })
    }

    fn apply(&self, command: Command) -> Option<State> {
        let handle = self.0.as_ptr();
        let (opcode, seconds) = match command {
            Command::Poll => (0, 0.0),
            Command::Play => (1, 0.0),
            Command::Pause => (2, 0.0),
            Command::Seek(seconds) => (3, seconds),
        };
        if opcode != 0 && !unsafe { paw_media_command(handle, opcode, seconds) } {
            return None;
        }
        let mut native = NativeState::default();
        if !unsafe { paw_media_state(handle, &mut native) } {
            return None;
        }
        let mode = match native.mode {
            0 => Mode::Loading,
            1 => Mode::Paused,
            2 => Mode::Buffering,
            3 => Mode::Playing,
            _ => return None,
        };
        Some(State {
            mode,
            position: native.position,
            duration: (native.duration > 0.0).then_some(native.duration),
            seeking: native.seeking,
        })
    }

    fn frame(&self, rgba: &mut [u8], mode: Mode) -> Result<Option<([u32; 2], usize)>, ()> {
        if mode == Mode::Loading {
            return Ok(None);
        }
        let (mut width, mut height) = (0, 0);
        // The bridge borrows the buffer for this call and bounds every pixel
        // write by the supplied capacity.
        let result = unsafe {
            paw_media_frame(
                self.0.as_ptr(),
                rgba.as_mut_ptr(),
                rgba.len(),
                VIDEO_EDGE,
                &mut width,
                &mut height,
            )
        };
        let size = [width, height];
        match result {
            0 => Ok(None),
            1 => frame_size(size)
                .filter(|&len| len <= rgba.len())
                .map(|len| Some((size, len)))
                .ok_or(()),
            _ => Err(()),
        }
    }

    fn metadata(&self) -> Metadata {
        let mut native = NativeMetadata::default();
        // The ready player stays owned by the helper throughout this call.
        unsafe { paw_media_metadata(self.0.as_ptr(), &mut native) };
        Metadata {
            video: native.has_video.then_some(VideoMetadata {
                dimensions: (native.width > 0 && native.height > 0)
                    .then_some([native.width, native.height]),
                codec: (native.video_codec != 0).then_some(native.video_codec),
                fps: positive(native.fps),
            }),
            audio: native.has_audio.then_some(AudioMetadata {
                codec: (native.audio_codec != 0).then_some(native.audio_codec),
                sample_rate: positive(native.sample_rate),
                channels: (native.channels > 0).then_some(native.channels),
            }),
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // The bridge balances the retained player and clears the active item.
        unsafe { paw_media_close(self.0.as_ptr()) };
    }
}
