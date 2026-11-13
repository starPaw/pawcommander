use eframe::egui;
use objc2::{
    define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject},
    sel, AnyThread, DefinedClass, MainThreadOnly,
};
use objc2_app_kit::{
    NSAboutPanelOptionApplicationIcon, NSAboutPanelOptionApplicationName, NSApplication, NSImage,
};
use objc2_foundation::{
    ns_string, MainThreadMarker, NSData, NSDictionary, NSMutableDictionary, NSObject, NSString,
};
use std::cell::OnceCell;

type AboutOptions = NSDictionary<NSString, AnyObject>;

struct MenuState {
    ctx: egui::Context,
    about_options: Retained<AboutOptions>,
}

fn request_quit(ctx: &egui::Context) {
    // AppKit's terminate: bypasses eframe's close-request handling. Always ask
    // the root window, even when Quick Look currently has keyboard focus.
    ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Close);
}

define_class!(
    // SAFETY: NSObject has no subclass requirements and AppKit calls menu actions on the main thread.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "PawCommanderMenuTarget"]
    #[ivars = MenuState]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(showPawCommanderAbout:))]
        fn show_about(&self, _: &AnyObject) {
            if let Some(mtm) = MainThreadMarker::new() {
                let app = NSApplication::sharedApplication(mtm);
                // SAFETY: The dictionary holds the image and name expected by AppKit.
                unsafe {
                    app.orderFrontStandardAboutPanelWithOptions(&self.ivars().about_options)
                };
            }
        }

        #[unsafe(method(requestPawCommanderQuit:))]
        fn request_quit(&self, _: &AnyObject) {
            request_quit(&self.ivars().ctx);
        }
    }
);

thread_local! {
    // NSMenuItem does not retain its target.
    static TARGET: OnceCell<Retained<MenuTarget>> = const { OnceCell::new() };
}

pub fn install(ctx: &egui::Context) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    // SAFETY: The native menu is accessed on the main thread after winit creates it.
    unsafe {
        let Some(menu) = app
            .mainMenu()
            .and_then(|bar| bar.itemArray().firstObject())
            .and_then(|item| item.submenu())
        else {
            return;
        };
        let items = menu.itemArray();
        let options =
            NSMutableDictionary::<NSString, AnyObject>::init(NSMutableDictionary::alloc());
        options.setObject_forKey(
            ns_string!("PawCommander"),
            ProtocolObject::from_ref(NSAboutPanelOptionApplicationName),
        );
        let data = NSData::with_bytes(include_bytes!("../assets/logo.png"));
        if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
            options.setObject_forKey(
                &image,
                ProtocolObject::from_ref(NSAboutPanelOptionApplicationIcon),
            );
        }
        let options = Retained::into_super(options);
        TARGET.with(|slot| {
            let target = slot.get_or_init(|| {
                let target = MenuTarget::alloc(mtm).set_ivars(MenuState {
                    ctx: ctx.clone(),
                    about_options: options,
                });
                msg_send![super(target), init]
            });
            if let Some(quit) = items
                .iter()
                .find(|item| item.action() == Some(sel!(terminate:)))
            {
                quit.setTarget(Some(target));
                quit.setAction(Some(sel!(requestPawCommanderQuit:)));
            }
            if let Some(about) = items
                .iter()
                .find(|item| item.action() == Some(sel!(orderFrontStandardAboutPanel:)))
            {
                about.setTitle(ns_string!("About PawCommander"));
                about.setTarget(Some(target));
                about.setAction(Some(sel!(showPawCommanderAbout:)));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_ui::ContextExt;

    #[test]
    fn quit_from_a_child_viewport_requests_root_close() {
        let ctx = egui::Context::default();
        let child = egui::ViewportId::from_hash_of("quick-look-test");
        let mut input = egui::RawInput {
            viewport_id: child,
            ..Default::default()
        };
        input.viewports.insert(child, Default::default());
        let output = ctx.run_test_ui(input, |ui| {
            assert_eq!(ui.ctx().viewport_id(), child);
            request_quit(ui.ctx());
        });
        assert_eq!(
            output.viewport_output[&egui::ViewportId::ROOT].commands,
            [egui::ViewportCommand::Close]
        );
        assert!(!output.viewport_output[&child]
            .commands
            .contains(&egui::ViewportCommand::Close));
    }
}
