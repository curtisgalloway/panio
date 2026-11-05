// Copyright 2026 The Panio Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// AVFoundation capture layer for hdmicap (macOS).
//
// Purpose-built replacement for the vendored nokhwa-bindings-macos fork.
// Design notes:
//
//   - macOS's UVC stack decodes MJPEG upstream of AVFoundation; only
//     uncompressed formats ('420v', 'yuvs') are reachable. We request the
//     device's highest-resolution format, preferring '420v' (bi-planar
//     4:2:0), whose contiguous luma plane the Rust side classifies directly
//     without any pixel conversion.
//
//   - The delegate compacts each frame (strips row padding) into a
//     double-buffered latest-frame slot under a mutex and signals a condvar.
//     The Rust capture thread blocks in avf_capture_wait_frame(), which
//     hands back a malloc'd copy the caller owns. Two memcpys per frame
//     (~1 ms at 8 MP) buys a clean ownership story across the FFI.
//
//   - NEVER set activeVideoMinFrameDuration / activeVideoMaxFrameDuration:
//     HDMI capture sticks (MS2109) throw NSException on those KVC paths.
//     That exception is why the nokhwa fork existed at all. We leave the
//     device's default frame pacing untouched and guard all session
//     configuration in @try/@catch.
//
// C ABI (mirrored by `mod avf_ffi` in capture.rs):
//
//   void avf_capture_enumerate(cb, ctx)
//   void *avf_capture_open(const char *unique_id, char *err, size_t errlen)
//   int avf_capture_wait_frame(void *h, uint64_t last_seq, uint32_t timeout_ms,
//                              AvfFrame *out)   // 1=frame 0=timeout -1=error
//   void avf_capture_frame_free(AvfFrame *f)
//   void avf_capture_close(void *h)

#import <AVFoundation/AVFoundation.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>

#include <pthread.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

// Pixel format tags handed to Rust. Wire values match the FourCCs so a
// debugger shows something recognizable.
#define AVF_PIXFMT_NV12 0x34323076  // '420v'
#define AVF_PIXFMT_YUYV 0x79757673  // 'yuvs'

typedef struct {
    uint64_t seq;
    uint32_t width;
    uint32_t height;
    uint32_t pixfmt;  // AVF_PIXFMT_*
    // NV12: y = compacted luma plane (w*h), cbcr = interleaved chroma
    // (w * h/2). YUYV: y = the whole packed buffer (w*h*2), cbcr = NULL.
    uint8_t *y;
    size_t y_len;
    uint8_t *cbcr;
    size_t cbcr_len;
} AvfFrame;

typedef void (*avf_enum_cb)(void *ctx, const char *name, const char *unique_id,
                            const char *misc);

// ── Delegate ─────────────────────────────────────────────────────────────────

@interface HdmicapDelegate : NSObject <AVCaptureVideoDataOutputSampleBufferDelegate> {
  @public
    pthread_mutex_t lock;
    pthread_cond_t cond;
    uint64_t seq;       // bumps on every stored frame; 0 = none yet
    BOOL failed;        // session runtime error; wakes waiters with -1
    uint32_t width, height, pixfmt;
    uint8_t *y;         // compacted latest planes, owned by the delegate
    size_t y_len, y_cap;
    uint8_t *cbcr;
    size_t cbcr_len, cbcr_cap;
}
@end

// Grow-only buffer helper: reuse the allocation across frames.
static int ensure_cap(uint8_t **buf, size_t *cap, size_t need) {
    if (*cap >= need) {
        return 1;
    }
    uint8_t *grown = realloc(*buf, need);
    if (!grown) {
        return 0;
    }
    *buf = grown;
    *cap = need;
    return 1;
}

// Copy `rows` rows of `row_bytes` from a possibly-padded plane into `dst`.
static void compact_plane(uint8_t *dst, const uint8_t *src, size_t rows,
                          size_t row_bytes, size_t src_stride) {
    if (src_stride == row_bytes) {
        memcpy(dst, src, rows * row_bytes);
        return;
    }
    for (size_t r = 0; r < rows; r++) {
        memcpy(dst + r * row_bytes, src + r * src_stride, row_bytes);
    }
}

@implementation HdmicapDelegate

- (instancetype)init {
    if ((self = [super init])) {
        pthread_mutex_init(&lock, NULL);
        pthread_cond_init(&cond, NULL);
    }
    return self;
}

- (void)dealloc {
    pthread_mutex_destroy(&lock);
    pthread_cond_destroy(&cond);
    free(y);
    free(cbcr);
}

- (void)captureOutput:(AVCaptureOutput *)output
    didOutputSampleBuffer:(CMSampleBufferRef)sampleBuffer
           fromConnection:(AVCaptureConnection *)connection {
    CVImageBufferRef img = CMSampleBufferGetImageBuffer(sampleBuffer);
    if (!img) {
        return;
    }
    if (CVPixelBufferLockBaseAddress(img, kCVPixelBufferLock_ReadOnly) !=
        kCVReturnSuccess) {
        return;
    }

    OSType fmt = CVPixelBufferGetPixelFormatType(img);
    size_t w = CVPixelBufferGetWidth(img);
    size_t h = CVPixelBufferGetHeight(img);

    pthread_mutex_lock(&lock);
    if (fmt == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange ||
        fmt == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange) {
        size_t need_y = w * h;
        size_t need_c = w * ((h + 1) / 2);
        if (ensure_cap(&y, &y_cap, need_y) && ensure_cap(&cbcr, &cbcr_cap, need_c)) {
            compact_plane(y, CVPixelBufferGetBaseAddressOfPlane(img, 0), h, w,
                          CVPixelBufferGetBytesPerRowOfPlane(img, 0));
            compact_plane(cbcr, CVPixelBufferGetBaseAddressOfPlane(img, 1),
                          (h + 1) / 2, w,
                          CVPixelBufferGetBytesPerRowOfPlane(img, 1));
            y_len = need_y;
            cbcr_len = need_c;
            width = (uint32_t)w;
            height = (uint32_t)h;
            pixfmt = AVF_PIXFMT_NV12;
            seq++;
            pthread_cond_broadcast(&cond);
        }
    } else if (fmt == kCVPixelFormatType_422YpCbCr8_yuvs ||
               fmt == kCVPixelFormatType_422YpCbCr8) {
        size_t need = w * 2 * h;
        if (ensure_cap(&y, &y_cap, need)) {
            compact_plane(y, CVPixelBufferGetBaseAddress(img), h, w * 2,
                          CVPixelBufferGetBytesPerRow(img));
            y_len = need;
            cbcr_len = 0;
            width = (uint32_t)w;
            height = (uint32_t)h;
            pixfmt = AVF_PIXFMT_YUYV;
            seq++;
            pthread_cond_broadcast(&cond);
        }
    }
    // Other formats: drop the frame; the session was configured for one of
    // the two above, so this only happens transiently if at all.
    pthread_mutex_unlock(&lock);

    CVPixelBufferUnlockBaseAddress(img, kCVPixelBufferLock_ReadOnly);
}

@end

// ── Session wrapper ──────────────────────────────────────────────────────────

@interface HdmicapCapture : NSObject {
  @public
    AVCaptureSession *session;
    HdmicapDelegate *delegate;
    dispatch_queue_t queue;
    id runtimeErrorObserver;
}
@end

@implementation HdmicapCapture
@end

static void set_err(char *err, size_t errlen, NSString *msg) {
    if (err && errlen > 0) {
        strlcpy(err, msg.UTF8String, errlen);
    }
}

static NSArray<AVCaptureDevice *> *all_video_devices(void) {
    AVCaptureDeviceDiscoverySession *ds = [AVCaptureDeviceDiscoverySession
        discoverySessionWithDeviceTypes:@[
            AVCaptureDeviceTypeExternal,
            AVCaptureDeviceTypeBuiltInWideAngleCamera
        ]
                              mediaType:AVMediaTypeVideo
                               position:AVCaptureDevicePositionUnspecified];
    return ds.devices;
}

void avf_capture_enumerate(avf_enum_cb cb, void *ctx) {
    @autoreleasepool {
        for (AVCaptureDevice *d in all_video_devices()) {
            NSString *misc =
                [NSString stringWithFormat:@"%@: %@", d.manufacturer, d.modelID];
            cb(ctx, d.localizedName.UTF8String, d.uniqueID.UTF8String,
               misc.UTF8String);
        }
    }
}

// Pick the device's highest-pixel-count format, preferring '420v' over
// 'yuvs' at equal resolution (contiguous luma plane, half the chroma bytes).
static AVCaptureDeviceFormat *pick_format(AVCaptureDevice *dev) {
    AVCaptureDeviceFormat *best = nil;
    uint64_t best_score = 0;
    for (AVCaptureDeviceFormat *f in dev.formats) {
        CMFormatDescriptionRef fd = f.formatDescription;
        if (CMFormatDescriptionGetMediaType(fd) != kCMMediaType_Video) {
            continue;
        }
        FourCharCode sub = CMFormatDescriptionGetMediaSubType(fd);
        if (sub != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange &&
            sub != kCVPixelFormatType_420YpCbCr8BiPlanarFullRange &&
            sub != kCVPixelFormatType_422YpCbCr8_yuvs) {
            continue;
        }
        CMVideoDimensions dims = CMVideoFormatDescriptionGetDimensions(fd);
        uint64_t score = (uint64_t)dims.width * (uint64_t)dims.height * 2;
        if (sub != kCVPixelFormatType_422YpCbCr8_yuvs) {
            score += 1;  // tie-break toward 420v
        }
        if (score > best_score) {
            best_score = score;
            best = f;
        }
    }
    return best;
}

void *avf_capture_open(const char *unique_id, char *err, size_t errlen) {
    @autoreleasepool {
        NSString *uid = [NSString stringWithUTF8String:unique_id];
        AVCaptureDevice *dev = nil;
        for (AVCaptureDevice *d in all_video_devices()) {
            if ([d.uniqueID isEqualToString:uid]) {
                dev = d;
                break;
            }
        }
        if (!dev) {
            set_err(err, errlen,
                    [NSString stringWithFormat:@"no device with id %@", uid]);
            return NULL;
        }

        AVCaptureDeviceFormat *fmt = pick_format(dev);
        if (!fmt) {
            set_err(err, errlen, @"device has no usable 420v/yuvs format");
            return NULL;
        }
        FourCharCode sub =
            CMFormatDescriptionGetMediaSubType(fmt.formatDescription);

        HdmicapCapture *cap = [[HdmicapCapture alloc] init];
        cap->delegate = [[HdmicapDelegate alloc] init];
        cap->queue = dispatch_queue_create("hdmicap.frames", DISPATCH_QUEUE_SERIAL);
        cap->session = [[AVCaptureSession alloc] init];

        @try {
            NSError *nserr = nil;
            [cap->session beginConfiguration];

            AVCaptureDeviceInput *input =
                [AVCaptureDeviceInput deviceInputWithDevice:dev error:&nserr];
            if (!input) {
                set_err(err, errlen, nserr.localizedDescription);
                return NULL;
            }
            if (![cap->session canAddInput:input]) {
                set_err(err, errlen, @"cannot add capture input");
                return NULL;
            }
            [cap->session addInput:input];

            // Set the format AFTER addInput so the session can't reset it.
            if (![dev lockForConfiguration:&nserr]) {
                set_err(err, errlen, nserr.localizedDescription);
                return NULL;
            }
            dev.activeFormat = fmt;
            // Deliberately NOT touching activeVideoMin/MaxFrameDuration:
            // MS2109-class HDMI sticks throw NSException on those setters.
            [dev unlockForConfiguration];

            AVCaptureVideoDataOutput *out = [[AVCaptureVideoDataOutput alloc] init];
            // Match the active format's pixel format exactly so the OS does
            // no extra conversion. The explicit width/height keys force
            // native-resolution delivery: the session's default preset
            // otherwise scales output to 1080p-class regardless of
            // activeFormat (iOS would use PresetInputPriority for this, but
            // that constant is unavailable on macOS).
            CMVideoDimensions dims =
                CMVideoFormatDescriptionGetDimensions(fmt.formatDescription);
            out.videoSettings = @{
                (id)kCVPixelBufferPixelFormatTypeKey : @(sub),
                (id)kCVPixelBufferWidthKey : @(dims.width),
                (id)kCVPixelBufferHeightKey : @(dims.height),
            };
            out.alwaysDiscardsLateVideoFrames = YES;
            [out setSampleBufferDelegate:cap->delegate queue:cap->queue];
            if (![cap->session canAddOutput:out]) {
                set_err(err, errlen, @"cannot add capture output");
                return NULL;
            }
            [cap->session addOutput:out];
            [cap->session commitConfiguration];
        } @catch (NSException *ex) {
            set_err(err, errlen,
                    [NSString stringWithFormat:@"session config: %@", ex.reason]);
            return NULL;
        }

        // Runtime errors (device yanked mid-stream, etc.) flip `failed` so
        // blocked waiters return -1 and the Rust reconnect loop takes over.
        HdmicapDelegate *del = cap->delegate;
        cap->runtimeErrorObserver = [[NSNotificationCenter defaultCenter]
            addObserverForName:AVCaptureSessionRuntimeErrorNotification
                        object:cap->session
                         queue:nil
                    usingBlock:^(NSNotification *__unused note) {
                      pthread_mutex_lock(&del->lock);
                      del->failed = YES;
                      pthread_cond_broadcast(&del->cond);
                      pthread_mutex_unlock(&del->lock);
                    }];

        [cap->session startRunning];
        return (void *)CFBridgingRetain(cap);
    }
}

int avf_capture_wait_frame(void *handle, uint64_t last_seq, uint32_t timeout_ms,
                           AvfFrame *out) {
    HdmicapCapture *cap = (__bridge HdmicapCapture *)handle;
    HdmicapDelegate *del = cap->delegate;

    struct timespec rel = {
        .tv_sec = timeout_ms / 1000,
        .tv_nsec = (long)(timeout_ms % 1000) * 1000000L,
    };

    pthread_mutex_lock(&del->lock);
    while (del->seq <= last_seq && !del->failed) {
        // Darwin extension: relative-time wait, no wallclock math.
        if (pthread_cond_timedwait_relative_np(&del->cond, &del->lock, &rel) != 0) {
            pthread_mutex_unlock(&del->lock);
            return 0;  // timeout
        }
    }
    if (del->failed) {
        pthread_mutex_unlock(&del->lock);
        return -1;
    }

    uint8_t *y_copy = malloc(del->y_len);
    uint8_t *c_copy = del->cbcr_len ? malloc(del->cbcr_len) : NULL;
    if (!y_copy || (del->cbcr_len && !c_copy)) {
        free(y_copy);
        free(c_copy);
        pthread_mutex_unlock(&del->lock);
        return -1;
    }
    memcpy(y_copy, del->y, del->y_len);
    if (del->cbcr_len) {
        memcpy(c_copy, del->cbcr, del->cbcr_len);
    }
    out->seq = del->seq;
    out->width = del->width;
    out->height = del->height;
    out->pixfmt = del->pixfmt;
    out->y = y_copy;
    out->y_len = del->y_len;
    out->cbcr = c_copy;
    out->cbcr_len = del->cbcr_len;
    pthread_mutex_unlock(&del->lock);
    return 1;
}

void avf_capture_frame_free(AvfFrame *f) {
    if (!f) {
        return;
    }
    free(f->y);
    free(f->cbcr);
    f->y = NULL;
    f->cbcr = NULL;
}

void avf_capture_close(void *handle) {
    @autoreleasepool {
        HdmicapCapture *cap = (HdmicapCapture *)CFBridgingRelease(handle);
        if (cap->runtimeErrorObserver) {
            [[NSNotificationCenter defaultCenter]
                removeObserver:cap->runtimeErrorObserver];
        }
        [cap->session stopRunning];
        // Wake any waiter so a blocked capture thread can observe shutdown.
        pthread_mutex_lock(&cap->delegate->lock);
        cap->delegate->failed = YES;
        pthread_cond_broadcast(&cap->delegate->cond);
        pthread_mutex_unlock(&cap->delegate->lock);
    }
}
