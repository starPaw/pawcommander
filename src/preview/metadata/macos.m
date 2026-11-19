#import <Foundation/Foundation.h>
#import <ImageIO/ImageIO.h>
#include <stddef.h>
#include <stdint.h>

enum { TextCapacity = 256 };

typedef struct {
    char make[TextCapacity];
    char model[TextCapacity];
    char lens[TextCapacity];
    char taken[TextCapacity];
    double iso;
    double aperture;
    double exposure;
    double focalLength;
} PawExif;

static void copy_text(id value, char *output) {
    if ([value isKindOfClass:NSString.class] &&
        ![value getCString:output maxLength:TextCapacity encoding:NSUTF8StringEncoding]) {
        output[0] = '\0';
    }
}

static double number(id value) {
    return [value isKindOfClass:NSNumber.class] ? [value doubleValue] : 0;
}

void paw_image_exif(const uint8_t *bytes, size_t length, PawExif *out) {
    @autoreleasepool {
        @try {
            NSData *data = [NSData dataWithBytesNoCopy:(void *)bytes length:length freeWhenDone:NO];
            CGImageSourceRef source = CGImageSourceCreateWithData((__bridge CFDataRef)data, NULL);
            if (source == NULL) {
                return;
            }
            NSDictionary *properties = CFBridgingRelease(
                CGImageSourceCopyPropertiesAtIndex(source, 0, NULL));
            CFRelease(source);

            NSDictionary *tiff = properties[(__bridge NSString *)kCGImagePropertyTIFFDictionary];
            if ([tiff isKindOfClass:NSDictionary.class]) {
                copy_text(tiff[(__bridge NSString *)kCGImagePropertyTIFFMake], out->make);
                copy_text(tiff[(__bridge NSString *)kCGImagePropertyTIFFModel], out->model);
            }
            NSDictionary *exif = properties[(__bridge NSString *)kCGImagePropertyExifDictionary];
            if ([exif isKindOfClass:NSDictionary.class]) {
                copy_text(exif[(__bridge NSString *)kCGImagePropertyExifLensModel], out->lens);
                copy_text(exif[(__bridge NSString *)kCGImagePropertyExifDateTimeOriginal], out->taken);
                id iso = exif[(__bridge NSString *)kCGImagePropertyExifISOSpeedRatings];
                out->iso = number([iso isKindOfClass:NSArray.class] ? [iso firstObject] : iso);
                out->aperture = number(exif[(__bridge NSString *)kCGImagePropertyExifFNumber]);
                out->exposure = number(exif[(__bridge NSString *)kCGImagePropertyExifExposureTime]);
                out->focalLength = number(exif[(__bridge NSString *)kCGImagePropertyExifFocalLength]);
            }
        } @catch (NSException *exception) {
            // Image decoding can still succeed when the metadata is unreadable.
        }
    }
}
