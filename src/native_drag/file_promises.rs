//! AppKit resolves SFTP items on a background queue after a receiver accepts the drop.
use crate::{
    file_error::{FilePromiseError, TransferPlanError},
    i18n::text,
    operations::TransferError,
    vfs::Location,
};
use eframe::egui;
use objc2::{
    define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject},
    AnyThread, DefinedClass,
};
use objc2_app_kit::{NSFilePromiseProvider, NSFilePromiseProviderDelegate};
use objc2_foundation::{
    NSCocoaErrorDomain, NSDictionary, NSError, NSLocalizedDescriptionKey, NSObject,
    NSObjectProtocol, NSOperationQueue, NSString, NSUserCancelledError, NSURL,
};
use std::{
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
    pub completion: Sender<Result<(), TransferError>>,
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
    fn request(&self, url: &NSURL) -> Result<(), TransferError> {
        if self.drag_result.load(Ordering::Acquire) == super::macos::DRAG_CANCELLED {
            return Err(TransferError::Cancelled);
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
            .map_err(|_| FilePromiseError::QueueDisconnected)?;
        self.ctx.request_repaint();
        result
            .recv()
            .map_err(|_| FilePromiseError::QueueDisconnected)?
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
            // AppKit already coordinates this write. A second claim on the
            // same URL can block Finder before the transfer reaches the queue.
            let error = self.ivars().request(url).err().map(promise_error);
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
    ) -> Result<Retained<NSFilePromiseProvider>, TransferError> {
        let name = source
            .path()
            .file_name()
            .ok_or_else(|| TransferPlanError::MissingName(source.clone()))?;
        let name = name
            .to_str()
            .ok_or_else(|| TransferPlanError::NonUtf8Name(source.clone()))?;
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

fn destination_path(url: &NSURL) -> Result<PathBuf, FilePromiseError> {
    if !url.isFileURL() {
        return Err(FilePromiseError::InvalidReceiverUrl);
    }
    let mut bytes = [0u8; libc::PATH_MAX as usize];
    let buffer = NonNull::from(&mut bytes[0]).cast();
    // SAFETY: the buffer is writable for the supplied length during the call.
    if !unsafe { url.getFileSystemRepresentation_maxLength(buffer, bytes.len()) } {
        return Err(FilePromiseError::InvalidReceiverUrl);
    }
    let encoded =
        CStr::from_bytes_until_nul(&bytes).map_err(|_| FilePromiseError::InvalidReceiverUrl)?;
    let path = PathBuf::from(OsStr::from_bytes(encoded.to_bytes()));
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(FilePromiseError::InvalidDestination(path));
    }
    Ok(path)
}

fn promise_error(error: TransferError) -> Retained<NSError> {
    let custom_domain = NSString::from_str("dev.pawcommander.file-promise");
    let (domain, code, reason) = if matches!(error, TransferError::Cancelled) {
        // SAFETY: Foundation keeps the error domain constant alive.
        (
            unsafe { NSCocoaErrorDomain },
            NSUserCancelledError,
            text!("Передача принимающему приложению отменена.").into(),
        )
    } else {
        (&*custom_domain, 1, crate::i18n::file_error(&error))
    };
    let description = NSString::from_str(&reason);
    let description: &AnyObject = &description;
    // SAFETY: NSError keeps the description object under its documented key.
    let info = unsafe { NSDictionary::from_slices(&[NSLocalizedDescriptionKey], &[description]) };
    unsafe { NSError::errorWithDomain_code_userInfo(domain, code, Some(&info)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn file_promise_uses_the_receivers_exact_file_url() {
        let file = NSURL::fileURLWithPath(&NSString::from_str("/private/tmp/a file (2).txt"));
        assert_eq!(
            destination_path(&file).unwrap(),
            Path::new("/private/tmp/a file (2).txt")
        );

        let root = NSURL::fileURLWithPath(&NSString::from_str("/"));
        assert!(matches!(
            destination_path(&root),
            Err(FilePromiseError::InvalidDestination(path)) if path == Path::new("/")
        ));
        let web =
            NSURL::URLWithString(&NSString::from_str("https://example.invalid/file")).unwrap();
        assert!(matches!(
            destination_path(&web),
            Err(FilePromiseError::InvalidReceiverUrl)
        ));
    }

    #[test]
    fn file_promise_distinguishes_user_cancellation_from_delivery_failure() {
        let cancelled = promise_error(TransferError::Cancelled);
        assert_eq!(cancelled.domain().to_string(), "NSCocoaErrorDomain");
        assert_eq!(cancelled.code(), NSUserCancelledError);

        let failure = promise_error(
            FilePromiseError::SourceMissing(PathBuf::from("/tmp/dragged.txt").into()).into(),
        );
        assert_eq!(
            failure.domain().to_string(),
            "dev.pawcommander.file-promise"
        );
        assert_eq!(failure.code(), 1);
        assert!(failure
            .localizedDescription()
            .to_string()
            .contains("/tmp/dragged.txt"));
    }
}
