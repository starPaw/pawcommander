// The source's AppKit tracking loop can defer Carbon hotkey callbacks until
// mouse-up. A short-lived listener receives just Escape on its own run loop.
#import <AppKit/AppKit.h>
#import <Carbon/Carbon.h>
#import <unistd.h>

static const EventHotKeyID dragEscape = {'PawD', 1};

static OSStatus escapePressed(__unused EventHandlerCallRef next, EventRef event,
                              __unused void *context) {
    EventHotKeyID id = {0};
    if (GetEventParameter(event, kEventParamDirectObject, typeEventHotKeyID,
                          NULL, sizeof id, NULL, &id) != noErr ||
        id.signature != dragEscape.signature || id.id != dragEscape.id)
        return eventNotHandledErr;
    if (NSEvent.pressedMouseButtons & 1) {
        const char byte = 'E';
        (void)write(STDOUT_FILENO, &byte, 1);
    }
    return noErr;
}

// The helper has no window, file URLs, or file operations. A closed control
// pipe (including parent exit) releases its system registration immediately.
int paw_drag_escape_listen(void) {
    @autoreleasepool {
        [NSApplication.sharedApplication setActivationPolicy:NSApplicationActivationPolicyProhibited];
        EventTypeSpec spec = {kEventClassKeyboard, kEventHotKeyPressed};
        EventTargetRef target = GetApplicationEventTarget();
        EventHotKeyRef key = NULL;
        if (InstallEventHandler(target, escapePressed, 1, &spec, NULL, NULL) != noErr ||
            RegisterEventHotKey(53, 0, dragEscape, target, 0, &key) != noErr)
            return 2;
        dispatch_async(dispatch_get_global_queue(QOS_CLASS_UTILITY, 0), ^{
            char byte;
            while (read(STDIN_FILENO, &byte, 1) > 0) {}
            _exit(0);
        });
        [NSApp run];
        UnregisterEventHotKey(key);
        return 0;
    }
}

@interface PawDragEscape : NSObject
@property(strong) NSFileHandle *lifetime;
@property BOOL active;
@end
@implementation PawDragEscape
@end

// AppKit explicitly permits postEvent:atStart: from a secondary thread.
// Do not dispatch this work to the blocked application event handler.
void *paw_drag_escape_start(NSInteger windowNumber) {
    PawDragEscape *guard = [PawDragEscape new];
    NSPipe *life = [NSPipe pipe], *events = [NSPipe pipe];
    NSTask *listener = [NSTask new];
    listener.executableURL = NSBundle.mainBundle.executableURL;
    listener.arguments = @[@"--paw-drag-escape-listener"];
    listener.standardInput = life;
    listener.standardOutput = events;
    if (![listener launchAndReturnError:NULL]) return NULL;
    guard.active = YES;
    guard.lifetime = life.fileHandleForWriting;
    NSFileHandle *input = events.fileHandleForReading;
    NSApplication *app = NSApp;
    dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INTERACTIVE, 0), ^{
        @autoreleasepool {
            char byte;
            while (read(input.fileDescriptor, &byte, 1) == 1) {
                if (byte != 'E') continue;
                @synchronized (guard) {
                    if (!guard.active) break;
                    NSEvent *escape = [NSEvent keyEventWithType:NSEventTypeKeyDown
                        location:NSZeroPoint modifierFlags:0 timestamp:NSProcessInfo.processInfo.systemUptime
                        windowNumber:windowNumber context:nil characters:@"\x1b" charactersIgnoringModifiers:@"\x1b"
                        isARepeat:NO keyCode:53];
                    [app postEvent:escape atStart:YES];
                }
            }
            // Reap our own helper off the UI thread after the drag finishes.
            [listener waitUntilExit];
        }
    });
    return (__bridge_retained void *)guard;
}

void paw_drag_escape_stop(PawDragEscape *guard) {
    @synchronized (guard) {
        if (!guard.active) return;
        guard.active = NO;
        [guard.lifetime closeFile];
        guard.lifetime = nil;
    }
}
