#[cfg(target_os = "macos")]
use crate::{file_error::SmbMutationAction, smb::MutationMount};
use crate::{
    fs_ops::{open_regular_source, LocalFileIdentity},
    preview::image_path_format,
    vfs::FileEntry,
};
use eframe::egui;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use std::{
    collections::HashMap,
    fs::{self, Metadata},
    io::{Cursor, Read},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
        Arc,
    },
    thread,
    time::SystemTime,
};

const CACHE_CAPACITY: usize = 256;
const MAX_PENDING: usize = 8;
const THUMBNAIL_EDGE: u32 = 128;
const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;
const MAX_IMAGE_EDGE: u32 = 8192;
const MAX_IMAGE_PIXELS: u64 = 16_000_000;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Version {
    len: u64,
    modified: Option<SystemTime>,
    identity: LocalFileIdentity,
}

impl Version {
    fn matches(self, metadata: &Metadata) -> bool {
        self.len == metadata.len()
            && self.modified == metadata.modified().ok()
            && Some(self.identity) == LocalFileIdentity::from_metadata(metadata)
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Source {
    path: PathBuf,
    version: Version,
    #[cfg(target_os = "macos")]
    smb_mount: Option<[i32; 2]>,
}

impl Source {
    fn for_entry(entry: &FileEntry) -> Option<Self> {
        let path = entry.path.local()?;
        let identity = entry.metadata.local_file?;
        if entry.size > MAX_FILE_BYTES || !image_path_format(path).is_some_and(supports_format) {
            return None;
        }
        Some(Self {
            path: path.to_owned(),
            version: Version {
                len: entry.size,
                modified: entry.modified,
                identity,
            },
            #[cfg(target_os = "macos")]
            smb_mount: entry.metadata.smb_mount,
        })
    }
}

struct Request {
    source: Source,
    cancelled: Arc<AtomicBool>,
}

struct Response {
    source: Source,
    image: Option<egui::ColorImage>,
}

struct Pending {
    visible: bool,
    cancelled: Arc<AtomicBool>,
}

struct Cached {
    texture: Option<egui::TextureHandle>,
    last_used: u64,
}

/// Shared by both panes. The pending limit covers queued, running and completed jobs.
pub(crate) struct Thumbnails {
    requests: SyncSender<Request>,
    responses: Receiver<Response>,
    pending: HashMap<Source, Pending>,
    cache: HashMap<Source, Cached>,
    wanted: Vec<Source>,
    frame: u64,
}

impl Thumbnails {
    pub(crate) fn new(ctx: &egui::Context) -> Self {
        let (thumbnails, requests, responses) = Self::channel();
        let ctx = ctx.clone();
        let _ = thread::Builder::new()
            .name("pawcommander-thumbnails".into())
            .spawn(move || {
                while let Ok(Request { source, cancelled }) = requests.recv() {
                    let image = if cancelled.load(Ordering::Relaxed) {
                        None
                    } else {
                        // A codec panic leaves this tile at its file icon.
                        std::panic::catch_unwind(|| load(&source)).ok().flatten()
                    };
                    if responses.send(Response { source, image }).is_err() {
                        break;
                    }
                    ctx.request_repaint_of(egui::ViewportId::ROOT);
                }
            });
        thumbnails
    }

    fn channel() -> (Self, Receiver<Request>, SyncSender<Response>) {
        let (requests, worker_requests) = mpsc::sync_channel(MAX_PENDING);
        let (worker_responses, responses) = mpsc::sync_channel(MAX_PENDING);
        (
            Self {
                requests,
                responses,
                pending: HashMap::new(),
                cache: HashMap::new(),
                wanted: Vec::new(),
                frame: 0,
            },
            worker_requests,
            worker_responses,
        )
    }

    pub(crate) fn begin_frame(&mut self) {
        self.frame += 1;
        self.wanted.clear();
        for pending in self.pending.values_mut() {
            pending.visible = false;
        }
    }

    pub(crate) fn refresh(&mut self) {
        self.cache.clear();
        for pending in self.pending.values() {
            pending.cancelled.store(true, Ordering::Relaxed);
        }
    }

    pub(crate) fn paint(&mut self, ui: &egui::Ui, entry: &FileEntry, rect: egui::Rect) -> bool {
        let Some(source) = Source::for_entry(entry) else {
            return false;
        };
        let Some(texture) = self.lookup(source) else {
            return false;
        };
        let size = texture.size_vec2();
        let scale = (rect.width() / size.x).min(rect.height() / size.y);
        ui.painter().image(
            texture.id(),
            egui::Rect::from_center_size(rect.center(), size * scale),
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
        true
    }

    fn lookup(&mut self, source: Source) -> Option<&egui::TextureHandle> {
        if let Some(entry) = self.cache.get_mut(&source) {
            entry.last_used = self.frame;
            return entry.texture.as_ref();
        }
        if let Some(pending) = self.pending.get_mut(&source) {
            pending.visible = true;
        } else if self.wanted.len() < MAX_PENDING && !self.wanted.contains(&source) {
            self.wanted.push(source);
        }
        None
    }

    pub(crate) fn finish_frame(&mut self, ctx: &egui::Context) {
        // Check visibility and version against the tiles just drawn, after the
        // directory refresh has had a chance to publish newer metadata.
        while let Ok(response) = self.responses.try_recv() {
            let Some(pending) = self.pending.remove(&response.source) else {
                continue;
            };
            if pending.visible
                && (pending.cancelled.load(Ordering::Relaxed) || self.cache_response(ctx, response))
            {
                ctx.request_repaint();
            }
        }
        for pending in self.pending.values() {
            if !pending.visible {
                pending.cancelled.store(true, Ordering::Relaxed);
            }
        }
        if self.wanted.is_empty() {
            return;
        }
        // If all slots are visible, remaining tiles keep their icons until
        // scrolling or navigation frees slots. Avoid re-decoding on every frame.
        let visible = self
            .cache
            .values()
            .filter(|entry| entry.last_used == self.frame)
            .count();
        let limit = (MAX_PENDING - self.pending.len())
            .min(CACHE_CAPACITY.saturating_sub(visible + self.pending.len()));
        for source in self.wanted.drain(..).take(limit) {
            let cancelled = Arc::new(AtomicBool::new(false));
            if self
                .requests
                .try_send(Request {
                    source: source.clone(),
                    cancelled: Arc::clone(&cancelled),
                })
                .is_err()
            {
                break;
            }
            self.pending.insert(
                source,
                Pending {
                    visible: true,
                    cancelled,
                },
            );
        }
    }

    fn cache_response(&mut self, ctx: &egui::Context, response: Response) -> bool {
        if self.cache.len() == CACHE_CAPACITY {
            let oldest = self
                .cache
                .iter()
                .filter(|(_, entry)| entry.last_used != self.frame)
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(source, _)| source.clone());
            let Some(oldest) = oldest else {
                return false;
            };
            self.cache.remove(&oldest);
        }
        let texture = response.image.map(|image| {
            ctx.load_texture("file-grid-thumbnail", image, egui::TextureOptions::LINEAR)
        });
        self.cache.insert(
            response.source,
            Cached {
                texture,
                last_used: self.frame,
            },
        );
        true
    }
}

fn load(source: &Source) -> Option<egui::ColorImage> {
    #[cfg(target_os = "macos")]
    let mount = {
        let mut mount = MutationMount::new(SmbMutationAction::ReadPreview);
        mount.check(&source.path).ok()?;
        if mount.id() != source.smb_mount {
            return None;
        }
        mount
    };
    let mut file = open_regular_source(&source.path).ok()?;
    #[cfg(target_os = "macos")]
    mount.verify_file(&file, &source.path).ok()?;
    let version = source.version;
    if !version.matches(&file.metadata().ok()?) {
        return None;
    }
    let mut bytes = Vec::with_capacity(version.len as usize);
    file.by_ref()
        .take(version.len + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 != version.len || !version.matches(&file.metadata().ok()?) {
        return None;
    }
    let thumbnail = decode(&bytes)?;
    // The handle may still refer to a file that was replaced at this path.
    if !version.matches(&fs::symlink_metadata(&source.path).ok()?) {
        return None;
    }
    #[cfg(target_os = "macos")]
    mount.recheck().ok()?;
    Some(thumbnail)
}

fn supports_format(format: ImageFormat) -> bool {
    matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif
    )
}

fn decode(bytes: &[u8]) -> Option<egui::ColorImage> {
    let format = image::guess_format(bytes).ok()?;
    if !supports_format(format) {
        return None;
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_EDGE);
    limits.max_image_height = Some(MAX_IMAGE_EDGE);
    limits.max_alloc = Some(128 * 1024 * 1024);
    let mut constructor_limits = limits.clone();
    if format == ImageFormat::Png {
        // The PNG reader retains this budget for its internal allocations
        // even when the wrapper's limits are updated below.
        constructor_limits.max_alloc = Some(16 * 1024 * 1024);
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(constructor_limits);
    let mut decoder = reader.into_decoder().ok()?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
        return None;
    }
    // Reserve the output before giving the remaining budget to the decoder.
    // Some formats only enforce the dimensions, so also cap pixel area above.
    limits.reserve(decoder.total_bytes()).ok()?;
    decoder.set_limits(limits).ok()?;
    let image = DynamicImage::from_decoder(decoder).ok()?;
    let image = if width > THUMBNAIL_EDGE || height > THUMBNAIL_EDGE {
        image.thumbnail(THUMBNAIL_EDGE, THUMBNAIL_EDGE)
    } else {
        image
    }
    .into_rgba8();
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [image.width() as usize, image.height() as usize],
        image.as_raw(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{self, Location};
    use std::{
        fs::{File, FileTimes},
        path::Path,
        time::{Duration, UNIX_EPOCH},
    };

    fn source(path: &Path) -> Option<Source> {
        vfs::read_automatic_directory(&path.parent().unwrap().to_owned().into(), &|| false)
            .unwrap()
            .unwrap()
            .into_iter()
            .find(|entry| entry.path.local() == Some(path))
            .and_then(|entry| Source::for_entry(&entry))
    }

    fn write_image(path: &Path, color: [u8; 3], size: [u32; 2], modified: u64) {
        image::RgbImage::from_pixel(size[0], size[1], image::Rgb(color))
            .save(path)
            .unwrap();
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(modified)))
            .unwrap();
    }

    fn draw(
        thumbnails: &mut Thumbnails,
        ctx: &egui::Context,
        visible: &[Source],
    ) -> Vec<Option<egui::TextureId>> {
        thumbnails.begin_frame();
        let textures = visible
            .iter()
            .map(|source| {
                thumbnails
                    .lookup(source.clone())
                    .map(egui::TextureHandle::id)
            })
            .collect();
        thumbnails.finish_frame(ctx);
        textures
    }

    #[test]
    fn supported_images_fit_the_thumbnail_and_other_files_keep_icons() {
        let directory = tempfile::tempdir().unwrap();
        for extension in ["PNG", "jpg", "gif"] {
            let path = directory.path().join(format!("image.{extension}"));
            write_image(&path, [220, 50, 15], [300, 150], 1_700_000_000);
            assert_eq!(load(&source(&path).unwrap()).unwrap().size, [128, 64]);
        }

        let path = directory.path().join("corrupt.png");
        fs::write(&path, b"not an image").unwrap();
        assert!(load(&source(&path).unwrap()).is_none());

        let path = directory.path().join("wide.png");
        write_image(&path, [220, 50, 15], [MAX_IMAGE_EDGE + 1, 1], 1_700_000_000);
        assert!(load(&source(&path).unwrap()).is_none());

        let mut gif = fs::read(directory.path().join("image.gif")).unwrap();
        // The GIF logical screen exceeds the pixel budget, while each edge fits.
        gif[6..8].copy_from_slice(&4_001_u16.to_le_bytes());
        gif[8..10].copy_from_slice(&4_000_u16.to_le_bytes());
        assert!(decode(&gif).is_none());

        let path = directory.path().join("large.png");
        File::create(&path)
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        assert!(source(&path).is_none());

        let path = directory.path().join("image.txt");
        fs::copy(directory.path().join("image.PNG"), &path).unwrap();
        assert!(source(&path).is_none());

        let path = directory.path().join("image.webp");
        write_image(&path, [220, 50, 15], [30, 15], 1_700_000_000);
        assert!(source(&path).is_none());
        let spoofed = directory.path().join("webp.png");
        fs::rename(path, &spoofed).unwrap();
        assert!(load(&source(&spoofed).unwrap()).is_none());

        let location = Location::Local(directory.path().to_owned());
        let mut entry = vfs::read_directory(&location, &|| false)
            .unwrap()
            .into_iter()
            .find(|entry| entry.name == "image.PNG")
            .unwrap();
        entry.path = Location::Archive {
            archive: Arc::new(directory.path().join("sample.zip")),
            path: "image.PNG".into(),
        };
        assert!(Source::for_entry(&entry).is_none());
    }

    #[test]
    fn directory_refresh_invalidates_a_cached_thumbnail() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image.png");
        let ctx = egui::Context::default();
        let (mut thumbnails, requests, responses) = Thumbnails::channel();
        write_image(&path, [255, 0, 0], [4, 2], 1_700_000_000);
        let before = source(&path).unwrap();
        let old = std::slice::from_ref(&before);
        assert_eq!(draw(&mut thumbnails, &ctx, old), [None]);
        let request = requests.try_recv().unwrap();
        let red = load(&request.source).unwrap();
        assert_eq!(red.pixels[0], egui::Color32::RED);
        responses
            .send(Response {
                source: request.source,
                image: Some(red),
            })
            .unwrap();
        draw(&mut thumbnails, &ctx, old);
        let old_texture = draw(&mut thumbnails, &ctx, old)[0].unwrap();

        write_image(&path, [0, 0, 255], [4, 2], 1_700_000_002);
        let after = source(&path).unwrap();
        assert!(before != after);
        let new = std::slice::from_ref(&after);
        assert_eq!(draw(&mut thumbnails, &ctx, new), [None]);
        let request = requests.try_recv().unwrap();
        let blue = load(&request.source).unwrap();
        assert_eq!(blue.pixels[0], egui::Color32::BLUE);
        responses
            .send(Response {
                source: request.source,
                image: Some(blue),
            })
            .unwrap();
        draw(&mut thumbnails, &ctx, new);
        let new_texture = draw(&mut thumbnails, &ctx, new)[0].unwrap();
        assert_ne!(old_texture, new_texture);
    }

    #[cfg(unix)]
    #[test]
    fn same_size_and_mtime_replacement_rejects_in_flight_result() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image.png");
        let replacement = directory.path().join("replacement.png");
        write_image(&path, [255, 0, 0], [4, 2], 1_700_000_000);
        let before = source(&path).unwrap();
        let ctx = egui::Context::default();
        let (mut thumbnails, requests, responses) = Thumbnails::channel();
        draw(&mut thumbnails, &ctx, std::slice::from_ref(&before));
        let request = requests.try_recv().unwrap();
        let old_pixels = load(&request.source);

        write_image(&replacement, [255, 0, 0], [4, 2], 1_700_000_000);
        fs::rename(replacement, &path).unwrap();
        let after = source(&path).unwrap();
        assert_eq!(before.version.len, after.version.len);
        assert_eq!(before.version.modified, after.version.modified);
        assert!(before.version != after.version);
        assert!(load(&request.source).is_none());
        responses
            .send(Response {
                source: request.source,
                image: old_pixels,
            })
            .unwrap();
        assert_eq!(
            draw(&mut thumbnails, &ctx, std::slice::from_ref(&after)),
            [None]
        );
        assert!(thumbnails.cache.is_empty());
        assert!(requests.try_recv().unwrap().source == after);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn smb_remount_separates_cached_and_in_flight_thumbnails() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image.png");
        write_image(&path, [255, 0, 0], [4, 2], 1_700_000_000);
        let local = source(&path).unwrap();
        let red = load(&local).unwrap();
        let cached = Source {
            smb_mount: Some([2, 3]),
            ..local.clone()
        };
        let in_flight = Source {
            path: directory.path().join("pending.png"),
            ..cached.clone()
        };
        let old = [cached.clone(), in_flight.clone()];
        let new = old.clone().map(|source| Source {
            smb_mount: Some([5, 8]),
            ..source
        });
        let ctx = egui::Context::default();
        let (mut thumbnails, requests, responses) = Thumbnails::channel();
        draw(&mut thumbnails, &ctx, &old);
        responses
            .send(Response {
                source: requests.try_recv().unwrap().source,
                image: Some(red.clone()),
            })
            .unwrap();
        let pending = requests.try_recv().unwrap();
        draw(&mut thumbnails, &ctx, &old);
        let old_texture = draw(&mut thumbnails, &ctx, &old)[0].unwrap();

        responses
            .send(Response {
                source: pending.source,
                image: Some(red),
            })
            .unwrap();
        assert_eq!(draw(&mut thumbnails, &ctx, &new), [None, None]);
        assert!(!thumbnails.cache.contains_key(&in_flight));
        let blue = egui::ColorImage::filled([4, 2], egui::Color32::BLUE);
        for expected in &new {
            let request = requests.try_recv().unwrap();
            assert!(request.source == *expected);
            responses
                .send(Response {
                    source: request.source,
                    image: Some(blue.clone()),
                })
                .unwrap();
        }
        draw(&mut thumbnails, &ctx, &new);
        let textures = draw(&mut thumbnails, &ctx, &new);
        assert_ne!(textures[0].unwrap(), old_texture);
        assert!(textures[1].is_some());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn thumbnail_for_missing_smb_mount_refuses_matching_local_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image.png");
        write_image(&path, [255, 0, 0], [4, 2], 1_700_000_000);
        let mut source = source(&path).unwrap();
        assert!(load(&source).is_some());

        source.smb_mount = Some([2, 3]);
        assert!(load(&source).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_is_skipped_even_when_it_replaces_a_listed_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image.png");
        let target = directory.path().join("target.png");
        write_image(&path, [255, 0, 0], [4, 2], 1_700_000_000);
        let before = source(&path).unwrap();
        fs::rename(&path, &target).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(source(&path).is_none());
        assert!(load(&before).is_none());
    }

    #[test]
    fn queue_is_bounded_and_deduplicates_across_panes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image.png");
        write_image(&path, [255, 0, 0], [4, 2], 1_700_000_000);
        let source = source(&path).unwrap();
        let visible: Vec<_> = (0..MAX_PENDING + 1)
            .map(|i| Source {
                path: directory.path().join(format!("{i}.png")),
                ..source.clone()
            })
            .flat_map(|source| [source.clone(), source])
            .collect();
        let ctx = egui::Context::default();
        let (mut thumbnails, requests, responses) = Thumbnails::channel();
        draw(&mut thumbnails, &ctx, &visible);
        assert_eq!(thumbnails.pending.len(), MAX_PENDING);
        let queued: Vec<_> = requests.try_iter().collect();
        assert_eq!(queued.len(), MAX_PENDING);

        // The scroll moved on before those results could be displayed.
        draw(&mut thumbnails, &ctx, &visible[visible.len() - 2..]);
        for request in queued {
            assert!(request.cancelled.load(Ordering::Relaxed));
            responses
                .send(Response {
                    source: request.source,
                    image: None,
                })
                .unwrap();
        }
        draw(&mut thumbnails, &ctx, &visible[visible.len() - 2..]);
        assert!(thumbnails.cache.is_empty());
        assert_eq!(thumbnails.pending.len(), 1);
        assert!(requests.try_recv().unwrap().source == *visible.last().unwrap());
    }

    #[test]
    fn explicit_refresh_retries_same_versions_across_directories() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("retry.png");
        let other_directory = tempfile::tempdir().unwrap();
        let other_path = other_directory.path().join("other.png");
        write_image(&path, [255, 0, 0], [4, 2], 1_700_000_000);
        write_image(&other_path, [0, 0, 255], [4, 2], 1_700_000_000);
        let retry = source(&path).unwrap();
        let other = source(&other_path).unwrap();
        let pending = Source {
            path: directory.path().join("pending.png"),
            ..retry.clone()
        };
        let visible = [retry.clone(), other.clone(), pending.clone()];
        let ctx = egui::Context::default();
        let (mut thumbnails, requests, responses) = Thumbnails::channel();
        draw(&mut thumbnails, &ctx, &visible);
        for _ in 0..2 {
            responses
                .send(Response {
                    source: requests.try_recv().unwrap().source,
                    image: None,
                })
                .unwrap();
        }
        let inflight = requests.try_recv().unwrap();
        draw(&mut thumbnails, &ctx, &visible);
        assert!(thumbnails.cache.contains_key(&retry));
        assert!(thumbnails.cache.contains_key(&other));

        thumbnails.refresh();
        assert!(thumbnails.cache.is_empty());
        assert!(inflight.cancelled.load(Ordering::Relaxed));
        responses
            .send(Response {
                source: inflight.source,
                image: load(&retry),
            })
            .unwrap();

        draw(&mut thumbnails, &ctx, &visible);
        assert!(!thumbnails.cache.contains_key(&pending));
        assert!(requests.try_recv().unwrap().source == retry);
        assert!(requests.try_recv().unwrap().source == other);
        draw(&mut thumbnails, &ctx, &visible);
        assert!(requests.try_recv().unwrap().source == pending);
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn cache_bounds_failures_and_evicts_oldest_entry_outside_the_view() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image.png");
        write_image(&path, [255, 0, 0], [4, 2], 1_700_000_000);
        let source = source(&path).unwrap();
        let visible: Vec<_> = (0..=CACHE_CAPACITY)
            .map(|i| Source {
                path: directory.path().join(format!("{i}.png")),
                ..source.clone()
            })
            .collect();
        let ctx = egui::Context::default();
        let (mut thumbnails, requests, responses) = Thumbnails::channel();
        for source in &visible[..CACHE_CAPACITY] {
            thumbnails.begin_frame();
            assert!(thumbnails.cache_response(
                &ctx,
                Response {
                    source: source.clone(),
                    image: None,
                },
            ));
        }
        // Slot 1 is now the oldest unused slot. Slot 0 remains in view.
        let selected = [visible[0].clone(), visible[CACHE_CAPACITY].clone()];
        draw(&mut thumbnails, &ctx, &selected);
        let request = requests.try_recv().unwrap();
        responses
            .send(Response {
                source: request.source,
                image: load(&source),
            })
            .unwrap();
        draw(&mut thumbnails, &ctx, &selected);
        assert_eq!(thumbnails.cache.len(), CACHE_CAPACITY);
        assert!(thumbnails.cache.contains_key(&selected[0]));
        assert!(!thumbnails.cache.contains_key(&visible[1]));
        assert!(thumbnails.cache[&selected[1]].texture.is_some());
        assert!(
            requests.try_recv().is_err(),
            "cached failures are not queued"
        );

        draw(&mut thumbnails, &ctx, &visible);
        assert!(requests.try_recv().is_err(), "all cache slots are visible");
    }
}
