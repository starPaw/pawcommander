#import <CoreGraphics/CoreGraphics.h>
#import <PDFKit/PDFKit.h>
#include <math.h>
#include <stdbool.h>
#include <stdint.h>

// These entry points are used only by the PDF helper process. Rust owns the
// input and output buffers, and each call bounds their lifetime explicitly.
void *paw_pdf_open(const uint8_t *bytes, size_t length, size_t *pages, bool *locked) {
    @autoreleasepool {
        @try {
            NSData *data = [NSData dataWithBytes:bytes length:length];
            PDFDocument *document = [[PDFDocument alloc] initWithData:data];
            *locked = document.isLocked;
            *pages = *locked ? 0 : document.pageCount;
            return (__bridge_retained void *)document;
        } @catch (NSException *exception) {
            return NULL;
        }
    }
}

void paw_pdf_close(void *handle) {
    @autoreleasepool {
        CFRelease(handle); // Balances __bridge_retained.
    }
}

int paw_pdf_contains(void *handle, size_t index, const uint8_t *word, size_t length) {
    @autoreleasepool {
        @try {
            PDFDocument *document = (__bridge PDFDocument *)handle;
            PDFPage *page = [document pageAtIndex:index];
            if (page == nil) {
                return -1;
            }
            NSString *query = [[NSString alloc] initWithBytes:word length:length encoding:NSUTF8StringEncoding];
            NSString *text = page.string;
            return text != nil && query.length != 0 &&
                [text rangeOfString:query options:NSCaseInsensitiveSearch].location != NSNotFound;
        } @catch (NSException *exception) {
            return -1;
        }
    }
}

static void highlight(PDFPage *page, CGContextRef context, NSString *query) {
    if (query.length == 0) {
        return;
    }
    NSString *text = page.string;
    CGContextSetRGBFillColor(context, 1, 0.79, 0.06, 0.36);
    const NSUInteger limit = 128;
    NSUInteger start = 0;
    for (NSUInteger matches = 0; matches < limit && start < text.length; matches++) {
        NSRange tail = NSMakeRange(start, text.length - start);
        NSRange hit = [text rangeOfString:query options:NSCaseInsensitiveSearch range:tail];
        if (hit.location == NSNotFound || hit.length == 0) {
            break;
        }
        PDFSelection *selection = [page selectionForRange:hit];
        for (PDFSelection *line in selection.selectionsByLine) {
            CGRect box = [line boundsForPage:page];
            if (!CGRectIsNull(box) && !CGRectIsInfinite(box) && !CGRectIsEmpty(box)) {
                CGContextFillRect(context, box);
            }
        }
        start = NSMaxRange(hit);
    }
}

static bool draw_page(PDFPage *page, CGPDFPageRef ref, uint8_t *pixels,
                      uint32_t width, uint32_t height, CGSize points, NSString *query) {
    CGColorSpaceRef colors = CGColorSpaceCreateDeviceRGB();
    if (colors == NULL) {
        return false;
    }
    CGContextRef context = CGBitmapContextCreate(pixels, width, height, 8, (size_t)width * 4, colors,
                                                 kCGBitmapByteOrder32Big | kCGImageAlphaPremultipliedLast);
    CGColorSpaceRelease(colors);
    if (context == NULL) {
        return false;
    }
    @try {
        CGRect target = CGRectMake(0, 0, width, height);
        CGContextSetRGBFillColor(context, 1, 1, 1, 1);
        CGContextFillRect(context, target);
        // Quartz centers a small PDF inside a larger drawing rectangle at
        // its original scale. Orient it in page points, then apply the
        // raster scale explicitly, including when rendering above 100%.
        CGFloat scale = fmin(width / points.width, height / points.height);
        CGContextTranslateCTM(context, (width - points.width * scale) / 2,
                              (height - points.height * scale) / 2);
        CGContextScaleCTM(context, scale, scale);
        CGRect page_rect = {CGPointZero, points};
        CGContextConcatCTM(context, CGPDFPageGetDrawingTransform(ref, kCGPDFCropBox, page_rect, 0, true));
        CGContextSaveGState(context);
        CGContextDrawPDFPage(context, ref);
        CGContextRestoreGState(context);
        highlight(page, context, query);
        return true;
    } @finally {
        CGContextRelease(context);
    }
}

bool paw_pdf_render(void *handle, size_t index, uint32_t edge, const uint8_t *word, size_t length,
                    uint8_t *pixels, size_t capacity, uint32_t *width, uint32_t *height,
                    double *point_width, double *point_height) {
    @autoreleasepool {
        @try {
            PDFDocument *document = (__bridge PDFDocument *)handle;
            PDFPage *page = [document pageAtIndex:index];
            CGPDFPageRef ref = page.pageRef;
            if (ref == NULL || edge == 0 || edge > 3072) {
                return false;
            }
            CGRect bounds = CGRectIntersection(CGPDFPageGetBoxRect(ref, kCGPDFCropBox),
                                               CGPDFPageGetBoxRect(ref, kCGPDFMediaBox));
            int rotation = CGPDFPageGetRotationAngle(ref);
            bool sideways = rotation % 180 != 0;
            double w = sideways ? bounds.size.height : bounds.size.width;
            double h = sideways ? bounds.size.width : bounds.size.height;
            if (!isfinite(w) || !isfinite(h) || w <= 0 || h <= 0 || w > 100000 || h > 100000) {
                return false;
            }
            double scale = edge / fmax(w, h);
            *width = (uint32_t)fmax(1, round(w * scale));
            *height = (uint32_t)fmax(1, round(h * scale));
            *point_width = w;
            *point_height = h;
            if ((size_t)*width * *height * 4 > capacity) {
                return false;
            }
            NSString *query = [[NSString alloc] initWithBytes:word length:length encoding:NSUTF8StringEncoding];
            return draw_page(page, ref, pixels, *width, *height, CGSizeMake(w, h), query);
        } @catch (NSException *exception) {
            return false;
        }
    }
}
