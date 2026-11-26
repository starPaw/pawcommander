// Finder on macOS 27 can reuse the previous source's file list when two
// AppKit processes produce the same draggingSequenceNumber. Let Finder
// inspect the selected URLs without granting a copy, then present them in
// a separate copy-only session under the same physical mouse press.
#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>

extern BOOL paw_drag_escape_was_pressed(id guard);

typedef NS_ENUM(NSUInteger, PawDragTarget) {
    PawDragSourceWindow, PawDragFinder, PawDragOther
};

// A named pasteboard lasts for the login session, including application
// relaunches. A drag from any other source changes the system drag board
// and invalidates this receipt. It contains no filenames or file contents.
static NSString *const PawFinderReceipt = @"dev.pawcommander.finder-drag-receipt";

static NSArray<NSNumber *> *previousFinderSequences(void) {
    NSPasteboard *receipt = [NSPasteboard pasteboardWithName:PawFinderReceipt];
    NSDictionary *value = [receipt propertyListForType:PawFinderReceipt];
    if (![value isKindOfClass:NSDictionary.class]) return nil;
    NSArray *sequences = value[@"sequences"];
    if (![sequences isKindOfClass:NSArray.class] || sequences.count > 4) return nil;
    for (NSNumber *number in sequences)
        if (![number isKindOfClass:NSNumber.class] || number.integerValue <= 0) return nil;
    NSPasteboard *board = [NSPasteboard pasteboardWithName:NSPasteboardNameDrag];
    NSRunningApplication *finder = [NSRunningApplication runningApplicationsWithBundleIdentifier:@"com.apple.finder"].firstObject;
    if (!finder || ![value[@"pid"] isEqual:@(finder.processIdentifier)] ||
        ![value[@"change"] isEqual:@(board.changeCount)]) return nil;
    return sequences;
}

static void rememberFinderSequences(NSDraggingSession *session, NSInteger revision, NSArray *sequences) {
    NSPasteboard *board = session.draggingPasteboard;
    NSRunningApplication *finder = [NSRunningApplication runningApplicationsWithBundleIdentifier:@"com.apple.finder"].firstObject;
    if (!finder || sequences.count > 4 || board.changeCount != revision ||
        ![board.name isEqualToString:NSPasteboardNameDrag]) return;
    NSDictionary *value = @{
        @"change": @(revision), @"pid": @(finder.processIdentifier), @"sequences": sequences,
    };
    NSPasteboard *receipt = [NSPasteboard pasteboardWithName:PawFinderReceipt];
    [receipt clearContents];
    [receipt setPropertyList:value forType:PawFinderReceipt];
}

@interface PawFinderURL : NSObject <NSPasteboardWriting>
@property(strong) NSURL *file;
@property(atomic) BOOL requested;
@end

@implementation PawFinderURL
- (NSArray<NSPasteboardType> *)writableTypesForPasteboard:(NSPasteboard *)board {
    return @[NSPasteboardTypeFileURL];
}
- (NSPasteboardWritingOptions)writingOptionsForType:(NSPasteboardType)type pasteboard:(NSPasteboard *)board {
    return NSPasteboardWritingPromised;
}
- (id)pasteboardPropertyListForType:(NSPasteboardType)type {
    self.requested = YES;
    return [self.file pasteboardPropertyListForType:type];
}
@end

@interface PawFinderDrag : NSObject <NSDraggingSource>
@property(strong) NSView *view;
@property(strong) NSEvent *press;
@property(strong) NSArray<NSDraggingItem *> *items;
@property(strong) id<NSDraggingSource> source;
@property(strong) id escape;
@property(strong) NSDraggingSession *session;
@property(strong) NSArray<PawFinderURL *> *urls;
@property NSInteger lastWindow;
@property PawDragTarget lastTarget;
@property BOOL preparing;
@property BOOL beganOverFinder;
@property BOOL finderReady;
@property BOOL offeredToFinder;
@property BOOL requested;
@property BOOL forFinder;
@property BOOL expired;
@property NSUInteger updates;
@property NSInteger revision;
@property(copy) NSArray<NSNumber *> *precedingSequences;
- (void)beginPreparing:(BOOL)preparing;
@end

@implementation PawFinderDrag
- (PawDragTarget)target {
    NSInteger window = [NSWindow windowNumberAtPoint:NSEvent.mouseLocation belowWindowWithWindowNumber:0];
    if (window == self.view.window.windowNumber) return PawDragSourceWindow;
    if (window == self.lastWindow) return self.lastTarget;
    // Only window ownership is needed. Coordinates stay in AppKit's screen space.
    NSArray *info = CFBridgingRelease(CGWindowListCopyWindowInfo(
        kCGWindowListOptionIncludingWindow, (CGWindowID)window));
    NSNumber *pid = info.firstObject[(__bridge NSString *)kCGWindowOwnerPID];
    if (!pid) return PawDragSourceWindow;
    NSRunningApplication *owner = [NSRunningApplication runningApplicationWithProcessIdentifier:pid.intValue];
    self.lastWindow = window;
    self.lastTarget = [owner.bundleIdentifier isEqualToString:@"com.apple.finder"] ? PawDragFinder : PawDragOther;
    return self.lastTarget;
}

- (void)beginPreparing:(BOOL)preparing {
    self.preparing = preparing;
    self.beganOverFinder = self.target == PawDragFinder;
    self.offeredToFinder = NO;
    self.requested = NO;
    self.expired = NO;
    self.updates = 0;
    NSArray *items = self.items;
    self.urls = nil;
    if (preparing) {
        NSMutableArray *writers = [NSMutableArray arrayWithCapacity:items.count];
        NSMutableArray *preview = [NSMutableArray arrayWithCapacity:items.count];
        for (NSDraggingItem *original in items) {
            PawFinderURL *writer = [PawFinderURL new];
            writer.file = (NSURL *)original.item;
            [writers addObject:writer];
            NSDraggingItem *item = [[NSDraggingItem alloc] initWithPasteboardWriter:writer];
            [item setDraggingFrame:original.draggingFrame contents:original.imageComponents.firstObject.contents];
            [preview addObject:item];
        }
        self.urls = writers;
        items = preview;
    }
    self.session = [self.view beginDraggingSessionWithItems:items event:self.press source:self];
    if (preparing) self.session.animatesToStartingPositionsOnCancelOrFail = NO;
}

- (void)cancelTracking {
    NSApplication *app = NSApp;
    NSEvent *cancel = [NSEvent keyEventWithType:NSEventTypeKeyDown location:NSZeroPoint
        modifierFlags:0 timestamp:NSProcessInfo.processInfo.systemUptime
        windowNumber:self.view.window.windowNumber context:nil characters:@"\x1b"
        charactersIgnoringModifiers:@"\x1b" isARepeat:NO keyCode:53];
    [app postEvent:cancel atStart:YES];
}

- (void)requestHandoffForFinder:(BOOL)forFinder {
    if (self.requested || !(NSEvent.pressedMouseButtons & 1)) return;
    self.requested = YES;
    self.forFinder = forFinder;
    self.session.animatesToStartingPositionsOnCancelOrFail = NO;
    [self cancelTracking];
}

- (NSDragOperation)draggingSession:(NSDraggingSession *)session sourceOperationMaskForDraggingContext:(NSDraggingContext)context {
    if (self.expired || [self.precedingSequences containsObject:@(session.draggingSequenceNumber)])
        return NSDragOperationNone;
    if (self.finderReady) return NSDragOperationCopy;
    if (context == NSDraggingContextOutsideApplication && self.target == PawDragFinder) {
        self.offeredToFinder = YES;
        return NSDragOperationGeneric;
    }
    return self.preparing ? NSDragOperationNone : NSDragOperationCopy;
}

- (void)updateStationaryPointer {
    if (!(NSEvent.pressedMouseButtons & 1)) return;
    // A held pointer may already be stationary in Finder when AppKit starts
    // tracking. Forward its current location to this session's tracking loop.
    NSWindow *window = self.view.window;
    NSEvent *motion = [NSEvent mouseEventWithType:NSEventTypeLeftMouseDragged
        location:[window convertPointFromScreen:NSEvent.mouseLocation] modifierFlags:NSEvent.modifierFlags
        timestamp:NSProcessInfo.processInfo.systemUptime windowNumber:window.windowNumber context:nil
        eventNumber:self.press.eventNumber clickCount:self.press.clickCount pressure:self.press.pressure];
    [NSApp postEvent:motion atStart:YES];
}

- (void)draggingSession:(NSDraggingSession *)session willBeginAtPoint:(NSPoint)point {
    self.revision = session.draggingPasteboard.changeCount;
    // AppKit enters tracking on a later run-loop turn. A release already
    // consumed before entry cannot wake that tracking loop a second time.
    if (!(NSEvent.pressedMouseButtons & 1)) {
        self.expired = YES;
        session.animatesToStartingPositionsOnCancelOrFail = NO;
        [self cancelTracking];
        return;
    }
    if (self.preparing) session.animatesToStartingPositionsOnCancelOrFail = NO;
    if (self.precedingSequences.count) {
        if ([self.precedingSequences containsObject:@(session.draggingSequenceNumber)]) {
            session.animatesToStartingPositionsOnCancelOrFail = NO;
            [self requestHandoffForFinder:YES];
            return;
        }
        self.finderReady = YES;
    }
    [self updateStationaryPointer];
}

- (void)draggingSession:(NSDraggingSession *)session movedToPoint:(NSPoint)point {
    [self.source draggingSession:session movedToPoint:point];
    if (self.finderReady) return;
    PawDragTarget target = self.target;
    if (target == PawDragFinder && self.offeredToFinder && self.preparing) {
        ++self.updates;
        // Finder can ask for the first file while entering the window. Give
        // it a tracking update after the read before offering the copy.
        if (self.updates >= 2 && self.urls.firstObject.requested) {
            [self requestHandoffForFinder:YES];
        } else if (self.updates <= 2) {
            [self updateStationaryPointer];
        }
    } else if ((target == PawDragFinder && (!self.preparing || !self.beganOverFinder)) ||
               (self.preparing && target == PawDragOther)) {
        [self requestHandoffForFinder:NO];
    }
}

- (void)finish:(NSDraggingSession *)session at:(NSPoint)point operation:(NSDragOperation)result {
    NSNumber *current = @(session.draggingSequenceNumber);
    if (result == NSDragOperationCopy && self.target == PawDragFinder) {
        rememberFinderSequences(session, self.revision, @[current]);
    } else if (self.precedingSequences.count) {
        // Escape or dropping elsewhere may leave Finder on either
        // selection. A subsequent drag must avoid both identifiers.
        NSMutableOrderedSet *possible = [NSMutableOrderedSet orderedSetWithArray:self.precedingSequences];
        [possible addObject:current];
        rememberFinderSequences(session, self.revision, possible.array);
    }
    [self.source draggingSession:session endedAtPoint:point operation:result];
}

- (void)draggingSession:(NSDraggingSession *)session endedAtPoint:(NSPoint)point operation:(NSDragOperation)operation {
    BOOL requested = self.requested;
    self.session = nil;
    if (requested && operation == NSDragOperationNone) {
        dispatch_async(dispatch_get_main_queue(), ^{
            if (self.view.window && (NSEvent.pressedMouseButtons & 1) && !paw_drag_escape_was_pressed(self.escape)) {
                PawDragTarget target = self.target;
                self.finderReady = self.forFinder && target == PawDragFinder;
                [self beginPreparing:!self.precedingSequences.count && !self.finderReady && target != PawDragOther];
            } else {
                [self finish:session at:point operation:NSDragOperationNone];
            }
        });
    } else {
        NSDragOperation result = self.expired || self.preparing || self.offeredToFinder ? NSDragOperationNone : operation;
        [self finish:session at:point operation:result];
    }
}
@end

void *paw_begin_file_drag(NSView *view, NSArray<NSDraggingItem *> *items, NSEvent *press,
                          id<NSDraggingSource> source, id escape, int localFiles) {
    if (localFiles && NSProcessInfo.processInfo.operatingSystemVersion.majorVersion == 27) {
        PawFinderDrag *drag = [PawFinderDrag new];
        drag.view = view;
        drag.items = items;
        drag.press = press;
        drag.source = source;
        drag.escape = escape;
        drag.lastWindow = -1;
        drag.precedingSequences = previousFinderSequences();
        [drag beginPreparing:!drag.precedingSequences.count && drag.target != PawDragOther];
        return (__bridge_retained void *)drag;
    }
    return (__bridge_retained void *)[view beginDraggingSessionWithItems:items event:press source:source];
}
