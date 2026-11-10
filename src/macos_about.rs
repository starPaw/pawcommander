use objc2::{
    declare_class, msg_send_id, mutability,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject},
    sel,
};
use objc2::{ClassType, DeclaredClass};
use objc2_app_kit::{
    NSAboutPanelOptionApplicationIcon, NSAboutPanelOptionApplicationName, NSApplication, NSImage,
};
use objc2_foundation::{
    ns_string, MainThreadMarker, NSData, NSDictionary, NSMutableDictionary, NSObject, NSString,
};
use std::cell::OnceCell;

type AboutOptions = NSDictionary<NSString, AnyObject>;

declare_class!(
    struct AboutTarget;

    // SAFETY: NSObject has no subclass requirements and AppKit calls menu actions on the main thread.
    unsafe impl ClassType for AboutTarget {
        type Super = NSObject;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "PawCommanderAboutTarget";
    }
    impl DeclaredClass for AboutTarget { type Ivars = Retained<AboutOptions>; }
    unsafe impl AboutTarget {
        #[method(showPawCommanderAbout:)]
        fn show_about(&self, _: &AnyObject) {
            if let Some(mtm) = MainThreadMarker::new() {
                let app = NSApplication::sharedApplication(mtm);
                // SAFETY: The dictionary holds the image and name expected by AppKit.
                unsafe { app.orderFrontStandardAboutPanelWithOptions(self.ivars()) };
            }
        }
    }
);

thread_local! {
    // NSMenuItem does not retain its target.
    static TARGET: OnceCell<Retained<AboutTarget>> = const { OnceCell::new() };
}

pub fn install() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    // SAFETY: The native menu is accessed on the main thread after winit creates it.
    unsafe {
        let Some(menu) = app
            .mainMenu()
            .and_then(|bar| bar.itemArray().first_retained())
            .and_then(|item| item.submenu())
        else {
            return;
        };
        let items = menu.itemArray();
        let Some(about) = items
            .iter()
            .find(|item| item.action() == Some(sel!(orderFrontStandardAboutPanel:)))
        else {
            return;
        };
        let data = NSData::with_bytes(include_bytes!("../assets/logo.png"));
        let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
            return;
        };
        let mut options =
            NSMutableDictionary::<NSString, AnyObject>::init(NSMutableDictionary::alloc());
        options.setObject_forKey(
            ns_string!("PawCommander"),
            ProtocolObject::from_ref(NSAboutPanelOptionApplicationName),
        );
        options.setObject_forKey(
            &image,
            ProtocolObject::from_ref(NSAboutPanelOptionApplicationIcon),
        );
        let options = Retained::into_super(options);
        TARGET.with(|slot| {
            let target = slot.get_or_init(|| {
                let target = mtm.alloc::<AboutTarget>().set_ivars(options);
                msg_send_id![super(target), init]
            });
            about.setTitle(ns_string!("About PawCommander"));
            about.setTarget(Some(target));
            about.setAction(Some(sel!(showPawCommanderAbout:)));
        });
    }
}
