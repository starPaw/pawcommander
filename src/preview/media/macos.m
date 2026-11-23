#import <AVFoundation/AVFoundation.h>
#import <CoreGraphics/CoreGraphics.h>
#import <CoreVideo/CoreVideo.h>
#import <QuartzCore/QuartzCore.h>
#import <UniformTypeIdentifiers/UniformTypeIdentifiers.h>
#include <errno.h>
#include <fcntl.h>
#include <math.h>
#include <stdbool.h>
#include <stdint.h>
#include <sys/stat.h>
#include <unistd.h>

@interface PawMediaFileSource : NSObject <AVAssetResourceLoaderDelegate> {
@public
    int descriptor;
    int64_t length;
    NSURL *url;
    NSString *contentType;
    dispatch_queue_t queue;
}
// A positive value is errno, -1 is EOF before the advertised file length.
@property(atomic) int32_t readFailure;
- (void)respondToRequest:(AVAssetResourceLoadingRequest *)request;
@end

@implementation PawMediaFileSource
- (BOOL)resourceLoader:(AVAssetResourceLoader *)loader
        shouldWaitForLoadingOfRequestedResource:(AVAssetResourceLoadingRequest *)request {
    if (![request.request.URL isEqual:url]) {
        return NO;
    }
    AVAssetResourceLoadingContentInformationRequest *info = request.contentInformationRequest;
    info.contentLength = length;
    info.contentType = contentType;
    info.byteRangeAccessSupported = YES;
    [self respondToRequest:request];
    return YES;
}

- (void)respondToRequest:(AVAssetResourceLoadingRequest *)request {
    if (request.cancelled) {
        return;
    }
    AVAssetResourceLoadingDataRequest *data = request.dataRequest;
    if (data == nil) {
        [request finishLoading];
        return;
    }
    int64_t offset = data.currentOffset;
    int64_t start = data.requestedOffset;
    if (start < 0 || offset < start || data.requestedLength < 0) {
        [request finishLoadingWithError:[NSError errorWithDomain:NSPOSIXErrorDomain code:EINVAL userInfo:nil]];
        return;
    }
    int64_t end = length;
    if (!data.requestsAllDataToEndOfResource && start < length) {
        end = start + MIN(data.requestedLength, length - start);
    }
    if (offset >= end) {
        [request finishLoading];
        return;
    }

    uint8_t bytes[64 * 1024];
    size_t size = (size_t)MIN((int64_t)sizeof(bytes), end - offset);
    ssize_t count;
    do {
        count = pread(descriptor, bytes, size, offset);
    } while (count < 0 && errno == EINTR && !request.cancelled);
    int error = count < 0 ? errno : EIO;
    if (request.cancelled) {
        return;
    }
    if (count <= 0) {
        if (self.readFailure == 0) {
            self.readFailure = count == 0 ? -1 : error;
        }
        [request finishLoadingWithError:[NSError errorWithDomain:NSPOSIXErrorDomain code:error userInfo:nil]];
        return;
    }
    [data respondWithData:[NSData dataWithBytes:bytes length:(NSUInteger)count]];
    if (offset + count == end) {
        [request finishLoading];
    } else {
        // Let cancellation and new seeks run between chunks of large requests.
        __weak PawMediaFileSource *source = self;
        dispatch_async(queue, ^{
            [source respondToRequest:request];
        });
    }
}
@end

// All player methods run on the helper's main thread. The only callback uses
// a weak reference and publishes seek completion back on that same thread.
@interface PawMedia : NSObject
@property(nonatomic, strong) AVPlayer *player;
@property(nonatomic, strong) AVPlayerItem *item;
@property(nonatomic, strong) AVPlayerItemVideoOutput *output;
// AVAssetResourceLoader holds its delegate weakly.
@property(nonatomic, strong) PawMediaFileSource *fileSource;
@property(nonatomic) CGAffineTransform orientation;
@property(nonatomic) bool hasOrientation;
@property(nonatomic) NSUInteger seekSerial;
@property(nonatomic) bool seeking;
@end

@implementation PawMedia
@end

typedef struct {
    double position;
    double duration;
    int32_t mode;
    bool seeking;
} PawMediaState;

typedef struct {
    double fps;
    double sampleRate;
    uint32_t width;
    uint32_t height;
    uint32_t videoCodec;
    uint32_t audioCodec;
    uint32_t channels;
    bool hasVideo;
    bool hasAudio;
} PawMediaMetadata;

typedef struct {
    int64_t code;
    uint8_t domain[128];
} PawMediaError;

static void *open_media(NSURL *url, PawMediaFileSource *source) {
    AVURLAsset *asset = [[AVURLAsset alloc] initWithURL:url options:@{
        AVURLAssetReferenceRestrictionsKey: @(AVAssetReferenceRestrictionForbidAll),
        AVURLAssetShouldSupportAliasDataReferencesKey: @NO,
    }];
    PawMedia *media = [[PawMedia alloc] init];
    if (source != nil) {
        media.fileSource = source;
        [asset.resourceLoader setDelegate:source queue:source->queue];
    }
    media.item = [AVPlayerItem playerItemWithAsset:asset];
    media.output = [[AVPlayerItemVideoOutput alloc] initWithPixelBufferAttributes:@{
        (__bridge NSString *)kCVPixelBufferPixelFormatTypeKey: @(kCVPixelFormatType_32BGRA),
    }];
    [media.item addOutput:media.output];
    media.player = [AVPlayer playerWithPlayerItem:media.item];
    media.player.allowsExternalPlayback = NO;
    return (__bridge_retained void *)media;
}

void *paw_media_open(const char *path) {
    @autoreleasepool {
        @try {
            NSURL *url = [NSURL fileURLWithFileSystemRepresentation:path isDirectory:NO relativeToURL:nil];
            return open_media(url, nil);
        } @catch (NSException *exception) {
            return NULL;
        }
    }
}

void *paw_media_open_descriptor(int32_t fd, const char *extension, int32_t *error) {
    @autoreleasepool {
        @try {
            struct stat st;
            // The process owns this descriptor until exit, including any
            // reads still completing on the delegate queue during teardown.
            if (fcntl(fd, F_SETFD, FD_CLOEXEC) < 0 || fstat(fd, &st) < 0) {
                *error = errno;
                return NULL;
            }
            if (!S_ISREG(st.st_mode) || st.st_size < 0) {
                return NULL;
            }
            NSString *suffix = [NSString stringWithUTF8String:extension];
            NSString *identifier = [UTType typeWithFilenameExtension:suffix].identifier;
            if (identifier == nil) {
                return NULL;
            }
            PawMediaFileSource *source = [[PawMediaFileSource alloc] init];
            source->descriptor = fd;
            source->length = st.st_size;
            source->url = [NSURL URLWithString:[@"paw-media://selected/media." stringByAppendingString:suffix]];
            source->contentType = identifier;
            source->queue = dispatch_queue_create("pawcommander.media.read",
                dispatch_queue_attr_make_with_autorelease_frequency(DISPATCH_QUEUE_SERIAL,
                    DISPATCH_AUTORELEASE_FREQUENCY_WORK_ITEM));
            return open_media(source->url, source);
        } @catch (NSException *exception) {
            return NULL;
        }
    }
}

int32_t paw_media_read_failure(void *handle) {
    PawMedia *media = (__bridge PawMedia *)handle;
    return media.fileSource.readFailure;
}

static void capture_media_error(NSError *error, PawMediaError *out) {
    NSString *domain = error.domain;
    NSUInteger length = domain.length;
    if (length == 0 || length >= sizeof(out->domain)) {
        return;
    }
    PawMediaError captured = {0};
    for (NSUInteger index = 0; index < length; index++) {
        unichar character = [domain characterAtIndex:index];
        if (!((character >= 'a' && character <= 'z') ||
              (character >= 'A' && character <= 'Z') ||
              (character >= '0' && character <= '9') ||
              character == '.' || character == '_' || character == '-')) {
            return;
        }
        captured.domain[index] = (uint8_t)character;
    }
    captured.code = error.code;
    *out = captured;
}

// The caller provides two zeroed slots for the error and its immediate cause.
void paw_media_failure(void *handle, PawMediaError out[2]) {
    @autoreleasepool {
        @try {
            PawMedia *media = (__bridge PawMedia *)handle;
            NSError *error = media.item.error ?: media.player.error;
            capture_media_error(error, &out[0]);
            id cause = error.userInfo[NSUnderlyingErrorKey];
            if ([cause isKindOfClass:NSError.class]) {
                capture_media_error(cause, &out[1]);
            }
        } @catch (NSException *exception) {
            // An unavailable diagnosis leaves the corresponding slot empty.
        }
    }
}

void paw_media_close(void *handle) {
    @autoreleasepool {
        PawMedia *media = (__bridge_transfer PawMedia *)handle;
        @try {
            [media.player pause];
            [media.item cancelPendingSeeks];
            [media.item removeOutput:media.output];
            [media.player replaceCurrentItemWithPlayerItem:nil];
        } @catch (NSException *exception) {
            // ARC still releases the player and its item.
        }
    }
}

void paw_media_pump(void) {
    @autoreleasepool {
        CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.003, false);
    }
}

static void seek(PawMedia *media, double seconds) {
    NSUInteger serial = ++media.seekSerial;
    media.seeking = true;
    [media.item cancelPendingSeeks];
    __weak PawMedia *target = media;
    CMTime time = CMTimeMakeWithSeconds(seconds, 600);
    [media.player seekToTime:time toleranceBefore:kCMTimeZero toleranceAfter:kCMTimeZero
        completionHandler:^(__unused BOOL finished) {
            dispatch_async(dispatch_get_main_queue(), ^{
                PawMedia *current = target;
                if (current != nil && current.seekSerial == serial) {
                    current.seeking = false;
                }
            });
        }];
}

bool paw_media_command(void *handle, int32_t command, double seconds) {
    @autoreleasepool {
        @try {
            PawMedia *media = (__bridge PawMedia *)handle;
            if (command == 1) {
                double position = CMTimeGetSeconds(media.player.currentTime);
                double duration = CMTimeGetSeconds(media.item.duration);
                if (!media.seeking && isfinite(duration) && position >= duration - 0.025) {
                    seek(media, 0);
                }
                [media.player play];
            } else if (command == 2) {
                [media.player pause];
            } else if (command == 3 && isfinite(seconds) && seconds >= 0) {
                double duration = CMTimeGetSeconds(media.item.duration);
                if (isfinite(duration) && duration > 0) {
                    seek(media, fmin(seconds, duration));
                }
            } else {
                return false;
            }
            return true;
        } @catch (NSException *exception) {
            return false;
        }
    }
}

bool paw_media_state(void *handle, PawMediaState *out) {
    @autoreleasepool {
        @try {
            PawMedia *media = (__bridge PawMedia *)handle;
            switch (media.item.status) {
                case AVPlayerItemStatusUnknown: out->mode = 0; break;
                case AVPlayerItemStatusReadyToPlay:
                    switch (media.player.timeControlStatus) {
                        case AVPlayerTimeControlStatusPaused: out->mode = 1; break;
                        case AVPlayerTimeControlStatusWaitingToPlayAtSpecifiedRate: out->mode = 2; break;
                        case AVPlayerTimeControlStatusPlaying: out->mode = 3; break;
                    }
                    break;
                case AVPlayerItemStatusFailed: return false;
            }
            if (media.player.status == AVPlayerStatusFailed) {
                return false;
            }
            double position = CMTimeGetSeconds(media.player.currentTime);
            double duration = CMTimeGetSeconds(media.item.duration);
            out->position = isfinite(position) ? fmax(0, position) : 0;
            out->duration = isfinite(duration) ? fmax(0, duration) : 0;
            out->seeking = media.seeking;
            return true;
        } @catch (NSException *exception) {
            return false;
        }
    }
}

void paw_media_metadata(void *handle, PawMediaMetadata *out) {
    @autoreleasepool {
        @try {
            PawMedia *media = (__bridge PawMedia *)handle;
            // The player is ready. Query each enabled track once in the helper.
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
            for (AVPlayerItemTrack *itemTrack in media.item.tracks) {
                if (!itemTrack.enabled) {
                    continue;
                }
                AVAssetTrack *track = itemTrack.assetTrack;
                if (!out->hasVideo && [track.mediaType isEqualToString:AVMediaTypeVideo]) {
                    out->hasVideo = true;
                    out->fps = track.nominalFrameRate;
                    CMFormatDescriptionRef format =
                        (__bridge CMFormatDescriptionRef)track.formatDescriptions.firstObject;
                    if (format != NULL) {
                        out->videoCodec = CMFormatDescriptionGetMediaSubType(format);
                        CMVideoDimensions size = CMVideoFormatDescriptionGetDimensions(format);
                        out->width = size.width > 0 ? (uint32_t)size.width : 0;
                        out->height = size.height > 0 ? (uint32_t)size.height : 0;
                    }
                } else if (!out->hasAudio && [track.mediaType isEqualToString:AVMediaTypeAudio]) {
                    out->hasAudio = true;
                    CMFormatDescriptionRef format =
                        (__bridge CMFormatDescriptionRef)track.formatDescriptions.firstObject;
                    if (format != NULL) {
                        out->audioCodec = CMFormatDescriptionGetMediaSubType(format);
                        const AudioStreamBasicDescription *description =
                            CMAudioFormatDescriptionGetStreamBasicDescription(format);
                        if (description != NULL) {
                            out->sampleRate = description->mSampleRate;
                            out->channels = description->mChannelsPerFrame;
                        }
                    }
                }
                if (out->hasVideo && out->hasAudio) {
                    break;
                }
            }
#pragma clang diagnostic pop
        } @catch (NSException *exception) {
            // Preserve the fields already read and let playback continue.
        }
    }
}

static CGAffineTransform orientation(PawMedia *media) {
    if (!media.hasOrientation) {
        media.orientation = CGAffineTransformIdentity;
        // The synchronous property is used only after the item is ready.
        // It remains available on the app's macOS 11 deployment target.
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
        for (AVPlayerItemTrack *itemTrack in media.item.tracks) {
            AVAssetTrack *track = itemTrack.assetTrack;
            if (itemTrack.enabled && [track.mediaType isEqualToString:AVMediaTypeVideo]) {
                media.orientation = track.preferredTransform;
                media.hasOrientation = true;
                break;
            }
        }
#pragma clang diagnostic pop
    }
    return media.orientation;
}

static bool copy_frame(PawMedia *media, CVPixelBufferRef pixel, uint8_t *rgba, size_t capacity,
                       uint32_t edge, uint32_t *width, uint32_t *height) {
    size_t w = CVPixelBufferGetWidth(pixel);
    size_t h = CVPixelBufferGetHeight(pixel);
    if (w == 0 || h == 0 || w > 16384 || h > 16384 || edge == 0 || edge > 1280 ||
        CVPixelBufferGetPixelFormatType(pixel) != kCVPixelFormatType_32BGRA ||
        CVPixelBufferLockBaseAddress(pixel, kCVPixelBufferLock_ReadOnly) != kCVReturnSuccess) {
        return false;
    }
    @try {
        const uint8_t *source = CVPixelBufferGetBaseAddress(pixel);
        size_t pitch = CVPixelBufferGetBytesPerRow(pixel);
        if (source == NULL || pitch < w * 4) {
            return false;
        }
        CGAffineTransform transform = orientation(media);
        CGRect box = CGRectApplyAffineTransform(CGRectMake(0, 0, w, h), transform);
        if (!isfinite(box.origin.x) || !isfinite(box.origin.y) ||
            !isfinite(box.size.width) || !isfinite(box.size.height) ||
            box.size.width <= 0 || box.size.height <= 0) {
            return false;
        }
        CGAffineTransform inverse = CGAffineTransformInvert(transform);
        double scale = fmin(1, edge / fmax(box.size.width, box.size.height));
        *width = (uint32_t)fmax(1, round(box.size.width * scale));
        *height = (uint32_t)fmax(1, round(box.size.height * scale));
        if ((size_t)*width * *height * 4 > capacity) {
            return false;
        }
        double stepX = inverse.a * box.size.width / *width;
        double stepY = inverse.b * box.size.width / *width;
        for (uint32_t y = 0; y < *height; y++) {
            CGPoint sample = CGPointApplyAffineTransform(
                CGPointMake(box.origin.x + 0.5 * box.size.width / *width,
                            box.origin.y + (y + 0.5) * box.size.height / *height), inverse);
            for (uint32_t x = 0; x < *width; x++, rgba += 4) {
                if (sample.x >= 0 && sample.y >= 0 && sample.x < w && sample.y < h) {
                    const uint8_t *bgra = source + (size_t)sample.y * pitch + (size_t)sample.x * 4;
                    rgba[0] = bgra[2];
                    rgba[1] = bgra[1];
                    rgba[2] = bgra[0];
                } else {
                    rgba[0] = rgba[1] = rgba[2] = 0;
                }
                rgba[3] = 255;
                sample.x += stepX;
                sample.y += stepY;
            }
        }
        return true;
    } @finally {
        CVPixelBufferUnlockBaseAddress(pixel, kCVPixelBufferLock_ReadOnly);
    }
}

int32_t paw_media_frame(void *handle, uint8_t *rgba, size_t capacity, uint32_t edge,
                        uint32_t *width, uint32_t *height) {
    @autoreleasepool {
        @try {
            PawMedia *media = (__bridge PawMedia *)handle;
            if (media.item.status != AVPlayerItemStatusReadyToPlay || media.seeking) {
                return 0;
            }
            CMTime time = [media.output itemTimeForHostTime:CACurrentMediaTime()];
            if (!CMTIME_IS_NUMERIC(time)) {
                time = media.player.currentTime;
            }
            if (![media.output hasNewPixelBufferForItemTime:time]) {
                return 0;
            }
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
            CVPixelBufferRef pixel = [media.output copyPixelBufferForItemTime:time itemTimeForDisplay:NULL];
#pragma clang diagnostic pop
            if (pixel == NULL) {
                return 0;
            }
            @try {
                return copy_frame(media, pixel, rgba, capacity, edge, width, height) ? 1 : -1;
            } @finally {
                CVPixelBufferRelease(pixel);
            }
        } @catch (NSException *exception) {
            return -1;
        }
    }
}
