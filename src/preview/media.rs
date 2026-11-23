mod session;
mod view;
mod worker;

use serde::{Deserialize, Serialize};
use std::{ffi::OsStr, path::Path};

pub(super) use view::Player;
pub(crate) use worker::run as run_media_worker;
pub(crate) const MEDIA_WORKER_ARG: &str = "--paw-media-preview-worker";
const VIDEO_EDGE: u32 = 1280;
const SFTP_LIMIT: u64 = 128 * 1024 * 1024;

pub(super) fn supported(path: &Path) -> bool {
    path.extension().and_then(media_extension).is_some()
}

fn media_extension(extension: &OsStr) -> Option<&'static str> {
    [
        "aac", "aif", "aiff", "caf", "flac", "m4a", "m4b", "mp3", "wav", "m4v", "mov", "mp4",
    ]
    .into_iter()
    .find(|name| extension.eq_ignore_ascii_case(name))
}

#[derive(Serialize, Deserialize)]
enum OpenSource {
    Path,
    Descriptor { fd: i32, extension: String },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
enum Command {
    Poll,
    Play,
    Pause,
    Seek(f64),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
enum Mode {
    #[default]
    Loading,
    Paused,
    Buffering,
    Playing,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
struct State {
    mode: Mode,
    position: f64,
    duration: Option<f64>,
    seeking: bool,
}

impl State {
    fn playing(self) -> bool {
        matches!(self.mode, Mode::Playing | Mode::Buffering)
    }

    fn valid(self) -> bool {
        self.position.is_finite()
            && self.position >= 0.0
            && self
                .duration
                .is_none_or(|value| value.is_finite() && value > 0.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Metadata {
    video: Option<VideoMetadata>,
    audio: Option<AudioMetadata>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct VideoMetadata {
    dimensions: Option<[u32; 2]>,
    codec: Option<u32>,
    fps: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct AudioMetadata {
    codec: Option<u32>,
    sample_rate: Option<f64>,
    channels: Option<u32>,
}

#[derive(Serialize, Deserialize)]
enum Reply {
    Opened,
    Snapshot {
        state: State,
        size: Option<[u32; 2]>,
        metadata: Option<Metadata>,
    },
    Failed,
    ReadError(i32),
    Truncated,
}

struct Frame {
    size: [u32; 2],
    rgba: Vec<u8>,
}

struct Snapshot {
    state: State,
    frame: Option<Frame>,
    metadata: Option<Metadata>,
}

fn frame_size([width, height]: [u32; 2]) -> Option<usize> {
    if width == 0 || height == 0 || width > VIDEO_EDGE || height > VIDEO_EDGE {
        return None;
    }
    Some(width as usize * height as usize * 4)
}

fn media_error() -> String {
    crate::i18n::text!("Не удалось воспроизвести медиафайл.").to_owned()
}
