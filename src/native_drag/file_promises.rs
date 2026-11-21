//! AppKit resolves SFTP items on a background queue after a receiver accepts the drop.
use crate::{i18n::text, vfs::Location};
use eframe::egui;
use objc2::{
    define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject},
    AnyThread, DefinedClass,
};
use objc2_app_kit::{NSFilePromiseProvider, NSFilePromiseProviderDelegate};
use objc2_foundation::{
    NSDictionary, NSError, NSFileCoordinator, NSFileCoordinatorWritingOptions,
    NSLocalizedDescriptionKey, NSObject, NSObjectProtocol, NSOperationQueue, NSString, NSURL,
};
use std::{
    cell::RefCell,
    ffi::{CStr, OsStr},
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    ptr::{self, NonNull},
    sync::{
        atomic::{AtomicU8, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc,
    },
};

#[derive(Clone, Copy)]
pub(crate) enum PromiseKind {
    File,
    Directory,
}

impl PromiseKind {
    fn content_type(self) -> &'static str {
        match self {
            Self::File => "public.data",
            Self::Directory => "public.folder",
        }
    }
}

pub(crate) struct PromiseRequest {
    pub source: Location,
    pub kind: PromiseKind,
    pub target: PathBuf,
    pub completion: Sender<Result<(), String>>,
}

struct PromiseState {
    source: Location,
    name: Retained<NSString>,
    kind: PromiseKind,
    queue: Retained<NSOperationQueue>,
    requests: Sender<PromiseRequest>,
    drag_result: Arc<AtomicU8>,
    ctx: egui::Context,
}

impl PromiseState {
    fn request(&self, url: &NSURL) -> Result<(), String> {
        if self.drag_result.load(Ordering::Acquire) == super::macos::DRAG_CANCELLED {
            return Err(text!("Передача принимающему приложению отменена.").into());
        }
        let target = destination_path(url)?;
        let (completion, result) = mpsc::channel();
        self.requests
            .send(PromiseRequest {
                source: self.source.clone(),
                kind: self.kind,
                target,
                completion,
            })
            .map_err(|_| cancelled())?;
        self.ctx.request_repaint();
        result.recv().map_err(|_| cancelled())?
    }

    fn write(&self, url: &NSURL) -> Result<(), Retained<NSError>> {
        if !url.isFileURL() {
            return Err(promise_error(invalid_destination()));
        }
        let result = RefCell::new(None);
        let writer = block2::RcBlock::new(|coordinated: NonNull<NSURL>| {
            // SAFETY: the coordinator supplies a live URL for the accessor call.
            *result.borrow_mut() = Some(self.request(unsafe { coordinated.as_ref() }));
        });
        let mut error = None;
        NSFileCoordinator::new().coordinateWritingItemAtURL_options_error_byAccessor(
            url,
            NSFileCoordinatorWritingOptions::empty(),
            Some(&mut error),
            &writer,
        );
        drop(writer);
        if let Some(error) = error {
            return Err(error);
        }
        result
            .into_inner()
            .unwrap_or_else(|| Err(invalid_destination()))
            .map_err(promise_error)
    }
}

define_class!(
    // SAFETY: the shared ivars are thread-safe and AppKit owns the callback arguments.
    #[unsafe(super(NSObject))]
    #[name = "PawCommanderSftpFilePromiseDelegate"]
    #[ivars = PromiseState]
    struct PromiseDelegate;

    unsafe impl NSObjectProtocol for PromiseDelegate {}
    unsafe impl NSFilePromiseProviderDelegate for PromiseDelegate {
        #[unsafe(method_id(filePromiseProvider:fileNameForType:))]
        fn filename(&self, _: &NSFilePromiseProvider, _: &NSString) -> Retained<NSString> {
            self.ivars().name.clone()
        }

        #[unsafe(method_id(operationQueueForFilePromiseProvider:))]
        fn queue(&self, _: &NSFilePromiseProvider) -> Retained<NSOperationQueue> {
            self.ivars().queue.clone()
        }

        #[unsafe(method(filePromiseProvider:writePromiseToURL:completionHandler:))]
        fn fulfill(
            &self,
            _: &NSFilePromiseProvider,
            url: &NSURL,
            done: &block2::DynBlock<dyn Fn(*mut NSError)>,
        ) {
            let error = self.ivars().write(url).err();
            done.call((error
                .as_ref()
                .map_or(ptr::null_mut(), |value| Retained::as_ptr(value).cast_mut()),));
        }
    }
);

pub(super) struct FilePromises {
    queue: Option<Retained<NSOperationQueue>>,
    send: Sender<PromiseRequest>,
    receive: Receiver<PromiseRequest>,
}

impl Default for FilePromises {
    fn default() -> Self {
        let (send, receive) = mpsc::channel();
        Self {
            queue: None,
            send,
            receive,
        }
    }
}

impl FilePromises {
    pub(super) fn receive(&self) -> Option<PromiseRequest> {
        self.receive.try_recv().ok()
    }

    pub(super) fn provider(
        &mut self,
        source: &Location,
        kind: PromiseKind,
        ctx: &egui::Context,
        drag_result: &Arc<AtomicU8>,
    ) -> Result<Retained<NSFilePromiseProvider>, String> {
        let name = source
            .path()
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| text!("Недопустимый путь файла").to_owned())?;
        let queue = self.queue.get_or_insert_with(|| {
            let queue = NSOperationQueue::new();
            queue.setMaxConcurrentOperationCount(1);
            queue
        });
        let delegate = PromiseDelegate::alloc().set_ivars(PromiseState {
            source: source.clone(),
            name: NSString::from_str(name),
            kind,
            queue: queue.clone(),
            requests: self.send.clone(),
            drag_result: drag_result.clone(),
            ctx: ctx.clone(),
        });
        // SAFETY: NSObject initializes the allocated subclass. The provider's
        // user info keeps the otherwise weak delegate alive through fulfillment.
        let delegate: Retained<PromiseDelegate> = unsafe { msg_send![super(delegate), init] };
        let provider = NSFilePromiseProvider::initWithFileType_delegate(
            NSFilePromiseProvider::alloc(),
            &NSString::from_str(kind.content_type()),
            ProtocolObject::from_ref(&*delegate),
        );
        unsafe { provider.setUserInfo(Some(&delegate)) };
        Ok(provider)
    }
}

fn destination_path(url: &NSURL) -> Result<PathBuf, String> {
    if !url.isFileURL() {
        return Err(invalid_destination());
    }
    // SAFETY: Foundation supplies a NUL-terminated filesystem representation.
    let bytes = unsafe { CStr::from_ptr(url.fileSystemRepresentation().as_ptr()) }.to_bytes();
    let path = PathBuf::from(OsStr::from_bytes(bytes));
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(invalid_destination());
    }
    Ok(path)
}

fn invalid_destination() -> String {
    text!("Принимающее приложение не указало путь к файлу.").into()
}

fn cancelled() -> String {
    text!("Передача принимающему приложению отменена.").into()
}

fn promise_error(reason: String) -> Retained<NSError> {
    let description = NSString::from_str(&reason);
    let description: &AnyObject = &description;
    // SAFETY: NSError keeps the description object under its documented key.
    let info = unsafe { NSDictionary::from_slices(&[NSLocalizedDescriptionKey], &[description]) };
    unsafe {
        NSError::errorWithDomain_code_userInfo(
            &NSString::from_str("dev.pawcommander.file-promise"),
            1,
            Some(&info),
        )
    }
}
