// Finder on macOS 27 can use an earlier file list when another process starts
// a drag with the same sequence number. Withhold all operations for that
// session: AppKit's external operation mask can apply to multiple destinations.
#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>

extern BOOL paw_drag_escape_was_pressed(id guard);

static NSString *const PawFinderReceipt = @"dev.pawcommander.finder-drag-receipt";

static BOOL validSequence(id value) {
    return [value isKindOfClass:NSNumber.class] && [value integerValue] > 0 &&
        [value integerValue] < NSIntegerMax;
}

static NSDictionary *knownReceipt(id value, pid_t pid) {
    if (![value isKindOfClass:NSDictionary.class] || ![value[@"pid"] isEqual:@(pid)]) return nil;
    NSArray *ids = value[@"sequences"], *ranges = value[@"ranges"] ?: @[];
    if (![ids isKindOfClass:NSArray.class] || ids.count > 4 ||
        ![ranges isKindOfClass:NSArray.class] || ranges.count > 1) return nil;
    for (id number in ids) if (!validSequence(number)) return nil;
    if (ranges.count) {
        id pair = ranges.firstObject;
        if (![pair isKindOfClass:NSArray.class] || [pair count] != 2 ||
            !validSequence(pair[0]) || !validSequence(pair[1]) ||
            [pair[0] integerValue] > [pair[1] integerValue]) return nil;
    }
    // Earlier versions also saved a pasteboard revision. A revision change
    // does not establish that Finder refreshed its file list.
    return value;
}

static BOOL receiptContains(NSDictionary *receipt, NSInteger number) {
    NSArray *range = [receipt[@"ranges"] firstObject];
    return [receipt[@"sequences"] containsObject:@(number)] ||
        (range && number >= [range[0] integerValue] && number <= [range[1] integerValue]);
}

static NSDictionary *receiptForOffer(pid_t pid, NSDictionary *prior, NSInteger number, BOOL confirmed) {
    if (confirmed) return @{@"pid": @(pid), @"sequences": @[@(number)]};
    if (receiptContains(prior, number)) return prior;
    NSArray *range = [prior[@"ranges"] firstObject];
    NSInteger low = range ? MIN(number, [range[0] integerValue]) : number;
    NSInteger high = range ? MAX(number, [range[1] integerValue]) : number;
    // Finder may still hold the confirmed drop or any subsequent offer it
    // saw before cancellation. A confirmed copy is the only reset point.
    return @{@"pid": @(pid), @"sequences": prior[@"sequences"] ?: @[], @"ranges": @[@[@(low), @(high)]]};
}

static NSDictionary *previousFinderReceipt(pid_t pid) {
    NSPasteboard *board = [NSPasteboard pasteboardWithName:PawFinderReceipt];
    return knownReceipt([board propertyListForType:PawFinderReceipt], pid);
}

static void rememberFinderSequence(pid_t pid, NSInteger number, BOOL confirmed) {
    if (number <= 0 || number >= NSIntegerMax) return;
    NSPasteboard *board = [NSPasteboard pasteboardWithName:PawFinderReceipt];
    NSDictionary *old = knownReceipt([board propertyListForType:PawFinderReceipt], pid);
    NSDictionary *value = receiptForOffer(pid, old, number, confirmed);
    if ([value isEqual:old]) return;
    [board clearContents];
    [board setPropertyList:value forType:PawFinderReceipt];
}

// Keep the selected URLs, image, and geometry independent of AppKit's
// single-session NSDraggingItems. A retry constructs new items for every row.
@interface PawFinderFile : NSObject
- (instancetype)initWithItem:(NSDraggingItem *)item;
- (NSDraggingItem *)newDraggingItem;
@end

@implementation PawFinderFile {
    NSURL *_url;
    NSRect _frame;
    id _image;
}
- (instancetype)initWithItem:(NSDraggingItem *)item {
    if ((self = [super init])) {
        _url = [item.item copy];
        _frame = item.draggingFrame;
        _image = item.imageComponents.firstObject.contents;
    }
    return self;
}
- (NSDraggingItem *)newDraggingItem {
    NSDraggingItem *item = [[NSDraggingItem alloc] initWithPasteboardWriter:_url];
    [item setDraggingFrame:_frame contents:_image];
    return item;
}
@end

@interface PawFinderDrag : NSObject <NSDraggingSource>
@property(strong) NSView *view;
@property(strong) NSEvent *press;
@property(strong) id<NSDraggingSource> source;
@property(strong) id escape;
@property(strong) NSDraggingSession *session;
@property(copy) NSArray<PawFinderFile *> *files;
@property(strong) NSRunningApplication *finder;
@property(copy) NSDictionary *previous;
@property(strong) NSMutableSet<NSNumber *> *rejected;
@property uint32_t releasesAtStart, keyDownsAtStart;
@property NSInteger lastWindow;
@property pid_t lastOwner;
@property BOOL began;
@property BOOL expired;
@property BOOL cancelledTracking;
@property BOOL offeredOutside;
@property BOOL recorded;
@property int recovery;
@end

@implementation PawFinderDrag
- (BOOL)stillHoldingGesture {
    return self.view.window && self.view.window.windowNumber == self.press.windowNumber &&
        (NSEvent.pressedMouseButtons & 1) &&
        CGEventSourceCounterForEventType(kCGEventSourceStateCombinedSessionState, kCGEventLeftMouseUp)
            == self.releasesAtStart && !paw_drag_escape_was_pressed(self.escape);
}

- (BOOL)ambiguous:(NSInteger)number {
    return number <= 1 || number == NSIntegerMax || receiptContains(self.previous, number) ||
        [self.rejected containsObject:@(number)];
}

- (void)stopTracking:(NSDraggingSession *)session {
    if (self.cancelledTracking) return;
    self.cancelledTracking = YES;
    session.animatesToStartingPositionsOnCancelOrFail = NO;
    // AppKit has no public cancel method on NSDraggingSession. This Escape is
    // local to the source application's tracking loop.
    NSEvent *cancel = [NSEvent keyEventWithType:NSEventTypeKeyDown location:NSZeroPoint
        modifierFlags:0 timestamp:NSProcessInfo.processInfo.systemUptime
        windowNumber:self.press.windowNumber context:nil characters:@"\x1b"
        charactersIgnoringModifiers:@"\x1b" isARepeat:NO keyCode:53];
    [NSApp postEvent:cancel atStart:YES];
}

// Returns zero when window ownership cannot be established at this point.
- (pid_t)ownerAtPoint:(NSPoint)point {
    NSInteger window = [NSWindow windowNumberAtPoint:point belowWindowWithWindowNumber:0];
    if (window <= 0) return 0;
    if (window == self.view.window.windowNumber) return NSProcessInfo.processInfo.processIdentifier;
    if (window != self.lastWindow || !self.lastOwner) {
        NSArray *info = CFBridgingRelease(CGWindowListCopyWindowInfo(
            kCGWindowListOptionIncludingWindow, (CGWindowID)window));
        self.lastOwner = [info.firstObject[(__bridge NSString *)kCGWindowOwnerPID] intValue];
        self.lastWindow = window;
    }
    return self.lastOwner;
}

- (void)recordOffer:(NSDraggingSession *)session at:(NSPoint)point
          uncertain:(BOOL)allowUnknown accepted:(BOOL)accepted {
    if ((!self.offeredOutside && !accepted) || (self.recorded && !accepted) ||
        !self.finder || self.finder.terminated) return;
    pid_t pid = self.finder.processIdentifier;
    pid_t owner = [self ownerAtPoint:point];
    if (owner == pid || (!owner && allowUnknown)) {
        rememberFinderSequence(pid, session.draggingSequenceNumber, accepted && owner == pid);
        self.recorded = YES;
    }
}

- (NSDragOperation)draggingSession:(NSDraggingSession *)session
    sourceOperationMaskForDraggingContext:(NSDraggingContext)context {
    if (!self.began && ![self stillHoldingGesture]) self.expired = YES;
    if (self.expired || [self ambiguous:session.draggingSequenceNumber]) {
        if (self.began) [self stopTracking:session];
        return NSDragOperationNone;
    }
    NSDragOperation mask = [self.source draggingSession:session sourceOperationMaskForDraggingContext:context];
    if (context == NSDraggingContextOutsideApplication && (mask & NSDragOperationCopy)) {
        self.offeredOutside = YES;
        [self recordOffer:session at:session.draggingLocation uncertain:NO accepted:NO];
    }
    return mask;
}

- (void)draggingSession:(NSDraggingSession *)session willBeginAtPoint:(NSPoint)point {
    self.began = YES;
    if (![self stillHoldingGesture]) self.expired = YES;
    if (self.expired || [self ambiguous:session.draggingSequenceNumber]) {
        [self stopTracking:session];
    }
}

- (void)draggingSession:(NSDraggingSession *)session movedToPoint:(NSPoint)point {
    [self.source draggingSession:session movedToPoint:point];
    [self recordOffer:session at:point uncertain:YES accepted:NO];
}

- (BOOL)beginWithItems:(NSArray<NSDraggingItem *> *)items {
    self.began = NO;
    self.expired = NO;
    self.cancelledTracking = NO;
    self.offeredOutside = NO;
    self.recorded = NO;
    self.session = [self.view beginDraggingSessionWithItems:items event:self.press source:self];
    return self.session != nil;
}

- (void)finish:(NSDraggingSession *)session at:(NSPoint)point operation:(NSDragOperation)operation
    recovery:(int)recovery {
    self.recovery = (recovery == 1 && paw_drag_escape_was_pressed(self.escape)) ? 0 : recovery;
    [self.source draggingSession:session endedAtPoint:point operation:operation];
}

- (void)draggingSession:(NSDraggingSession *)session endedAtPoint:(NSPoint)point
    operation:(NSDragOperation)operation {
    self.session = nil;
    NSInteger number = session.draggingSequenceNumber;
    BOOL ambiguous = [self ambiguous:number];
    if (!ambiguous && !self.expired) {
        [self recordOffer:session at:point uncertain:YES accepted:(operation == NSDragOperationCopy)];
        [self finish:session at:point operation:operation recovery:0];
        return;
    }
    if (operation != NSDragOperationNone) {
        // AppKit reported a transfer despite our cancellation or refusal.
        // Preserve any earlier candidate even when the pointer is now in Finder.
        self.offeredOutside = YES;
        [self recordOffer:session at:point uncertain:YES accepted:NO];
        [self finish:session at:point operation:NSDragOperationNone recovery:2];
        return;
    }
    if (!ambiguous || number <= 0 || number == NSIntegerMax ||
        self.rejected.count >= 2 || [self.rejected containsObject:@(number)] ||
        ![self stillHoldingGesture]) {
        [self finish:session at:point operation:NSDragOperationNone recovery:(ambiguous ? 1 : 0)];
        return;
    }
    [self.rejected addObject:@(number)];
    // Only start another tracking loop after the old one has unwound. Count
    // physical releases too: a new press must never revive the old selection.
    CFRunLoopPerformBlock(CFRunLoopGetMain(), kCFRunLoopDefaultMode, ^{
        if ([self stillHoldingGesture] &&
            CGEventSourceCounterForEventType(kCGEventSourceStateCombinedSessionState, kCGEventKeyDown)
                == self.keyDownsAtStart) {
            NSMutableArray *items = [NSMutableArray arrayWithCapacity:self.files.count];
            for (PawFinderFile *file in self.files) [items addObject:[file newDraggingItem]];
            if ([self beginWithItems:items]) return;
        }
        [self finish:session at:point operation:NSDragOperationNone recovery:1];
    });
    CFRunLoopWakeUp(CFRunLoopGetMain());
}
@end

int paw_file_drag_recovery(id token) {
    return [token isKindOfClass:PawFinderDrag.class] ? [(PawFinderDrag *)token recovery] : 0;
}

void *paw_begin_file_drag(NSView *view, NSArray<NSDraggingItem *> *items, NSEvent *press,
                          id<NSDraggingSource> source, id escape, int localFiles) {
    if (!localFiles || NSProcessInfo.processInfo.operatingSystemVersion.majorVersion != 27)
        return (__bridge_retained void *)[view beginDraggingSessionWithItems:items event:press source:source];
    PawFinderDrag *drag = [PawFinderDrag new];
    drag.view = view;
    drag.press = press;
    drag.source = source;
    drag.escape = escape;
    drag.finder = [NSRunningApplication runningApplicationsWithBundleIdentifier:@"com.apple.finder"].firstObject;
    drag.previous = previousFinderReceipt(drag.finder.processIdentifier);
    drag.rejected = [NSMutableSet set];
    drag.releasesAtStart = CGEventSourceCounterForEventType(kCGEventSourceStateCombinedSessionState, kCGEventLeftMouseUp);
    drag.keyDownsAtStart = CGEventSourceCounterForEventType(kCGEventSourceStateCombinedSessionState, kCGEventKeyDown);
    NSMutableArray *files = [NSMutableArray arrayWithCapacity:items.count];
    for (NSDraggingItem *item in items) [files addObject:[[PawFinderFile alloc] initWithItem:item]];
    drag.files = files;
    return [drag beginWithItems:items] ? (__bridge_retained void *)drag : NULL;
}
