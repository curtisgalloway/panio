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

// hid_capture_usb.m
//
// Capture a USB HID device away from the macOS HID stack entirely and read
// its raw interrupt-IN reports with timestamps.
//
// Why: on the DriverKit HID stack (macOS 12+), IOHIDDeviceOpen with
// kIOHIDOptionsTypeSeizeDevice is NOT exclusive — AppleUserHIDEventDriver
// keeps feeding the system event path, so injected reports leak into the
// live session (observed: the real cursor moves). The supported way to get
// true exclusivity is IOUSBHost whole-device capture
// (IOUSBHostObjectInitOptionsDeviceCapture), which re-enumerates the device
// with every kernel/dext driver detached. Root passes the entitlement gate.
//
// The capture detaches the WHOLE device: while this tool runs, nothing else
// services the device, so it must be running (and reading) before any HID
// injection — the board's send_report stalls otherwise.
//
// On exit the device is released and re-enumerates; the HID dext re-binds
// and it becomes a live keyboard/mouse to this Mac again.
//
// Build:
//   clang -fobjc-arc -framework Foundation -framework IOKit \
//         -framework IOUSBHost -o hid_capture_usb hid_capture_usb.m
//
// Run:
//   sudo ./hid_capture_usb [serial]
//
// Output per report (same format as hid_seize_reports):
//   report ts=<sec.usec> dt=<usec since previous, -1 first> len=<n>: <hex>
// The first payload byte is the report ID (the device uses numbered reports).

#import <Foundation/Foundation.h>
#import <IOKit/IOKitLib.h>
#import <IOUSBHost/IOUSBHost.h>
#include <time.h>

// KB2040 running CircuitPython HID injector firmware.
static const long kVendorID = 0x239A;
static const long kProductID = 0x8106;
// The injector board; there may be other KB2040s attached (e.g. a macro
// pad with the same VID/PID), so default to this serial. Override via argv.
static NSString *const kDefaultSerial = @"DF643CF013231635";

static IOUSBHostDevice *gDevice = nil;

static void HandleSignal(int sig) {
    (void)sig;
    // Releasing the device re-enumerates it; the HID dext re-binds.
    if (gDevice) [gDevice destroy];
    fprintf(stderr, "\nReleased device — it is a live HID to this Mac again.\n");
    exit(0);
}

// Find the IOUSBHostDevice IOService for VID/PID + serial. VID/PID go in the
// matching dictionary; the serial is checked against each candidate's
// registry properties (avoids matching-dict key subtleties).
static io_service_t FindDevice(NSString *serial) {
    NSMutableDictionary *match =
        (__bridge_transfer NSMutableDictionary *)IOServiceMatching("IOUSBHostDevice");
    match[@"idVendor"] = @(kVendorID);
    match[@"idProduct"] = @(kProductID);

    io_iterator_t iter = IO_OBJECT_NULL;
    kern_return_t kr = IOServiceGetMatchingServices(
        kIOMainPortDefault, (__bridge_retained CFDictionaryRef)match, &iter);
    if (kr != KERN_SUCCESS) return IO_OBJECT_NULL;

    io_service_t found = IO_OBJECT_NULL;
    io_service_t svc;
    while ((svc = IOIteratorNext(iter))) {
        NSString *sn = (__bridge_transfer NSString *)IORegistryEntryCreateCFProperty(
            svc, CFSTR("USB Serial Number"), kCFAllocatorDefault, 0);
        if ([sn isEqualToString:serial]) {
            found = svc;
            break;
        }
        IOObjectRelease(svc);
    }
    IOObjectRelease(iter);
    return found;
}

// Walk the configuration descriptor for the first interrupt-IN endpoint.
static const IOUSBEndpointDescriptor *FindInterruptIn(
    const IOUSBConfigurationDescriptor *config) {
    const IOUSBDescriptorHeader *d = NULL;
    while ((d = IOUSBGetNextDescriptor(config, d))) {
        if (d->bDescriptorType != kIOUSBDescriptorTypeEndpoint) continue;
        const IOUSBEndpointDescriptor *ep = (const IOUSBEndpointDescriptor *)d;
        if ((ep->bmAttributes & kIOUSBEndpointDescriptorTransferType) ==
                kIOUSBEndpointDescriptorTransferTypeInterrupt &&
            (ep->bEndpointAddress & kIOUSBEndpointDescriptorDirectionIn)) {
            return ep;
        }
    }
    return NULL;
}

int main(int argc, char **argv) {
    @autoreleasepool {
        NSString *serial =
            argc > 1 ? [NSString stringWithUTF8String:argv[1]] : kDefaultSerial;

        io_service_t svc = FindDevice(serial);
        if (!svc) {
            fprintf(stderr, "no USB device %04lX:%04lX serial %s\n", kVendorID,
                    kProductID, serial.UTF8String);
            return 1;
        }

        NSError *err = nil;
        gDevice = [[IOUSBHostDevice alloc]
            initWithIOService:svc
                      options:IOUSBHostObjectInitOptionsDeviceCapture
                        queue:nil
                        error:&err
              interestHandler:nil];
        IOObjectRelease(svc);
        if (!gDevice) {
            fprintf(stderr, "device capture failed: %s\n",
                    err.description.UTF8String);
            return 1;
        }
        signal(SIGINT, HandleSignal);
        signal(SIGTERM, HandleSignal);

        // The capture re-enumerated the device with no drivers attached, so
        // nothing has configured it — select configuration 1. matchInterfaces:YES
        // republishes the interface nub so we can wrap it; the whole-device
        // capture keeps system HID dexts from re-binding to it (verified: the
        // device drops off `hidutil list` while this tool runs).
        if (![gDevice configureWithValue:1 matchInterfaces:YES error:&err]) {
            fprintf(stderr, "configure failed: %s\n", err.description.UTF8String);
            return 1;
        }

        // Diagnostic: hold the capture and sleep so `hidutil list` can be
        // inspected while the device is captured (does capture alone detach the
        // HID stack?). Enable with HID_CAPTURE_PROBE=1.
        if (getenv("HID_CAPTURE_PROBE")) {
            printf("PROBE: device captured + configured; sleeping. "
                   "Check `hidutil list` now.\n");
            fflush(stdout);
            for (;;) pause();
        }

        const IOUSBConfigurationDescriptor *config =
            [gDevice configurationDescriptorWithConfigurationValue:1 error:&err];
        if (!config) {
            fprintf(stderr, "no config descriptor: %s\n", err.description.UTF8String);
            return 1;
        }
        const IOUSBEndpointDescriptor *ep = FindInterruptIn(config);
        if (!ep) {
            fprintf(stderr, "no interrupt-IN endpoint found\n");
            return 1;
        }
        uint16_t maxPacket = ep->wMaxPacketSize;

        // The interrupt-IN completion handler fires on the interface's dispatch
        // queue, so the interface needs a real serial queue (not nil).
        dispatch_queue_t q = dispatch_queue_create("hidcap.read", DISPATCH_QUEUE_SERIAL);

        // Find the interface nub as a CHILD of *our* captured device — NOT via
        // a global VID/PID match, because a second KB2040 (e.g. a macro pad)
        // with the same VID/PID would match too and its interface is still
        // owned by the system HID dext, so the exclusive open would fail. The
        // nub publishes asynchronously after configureWithValue:, so retry.
        IOUSBHostInterface *intf = nil;
        for (int tries = 0; tries < 100 && !intf; tries++) {
            io_iterator_t childIter = IO_OBJECT_NULL;
            if (IORegistryEntryGetChildIterator(gDevice.ioService, kIOServicePlane,
                                                &childIter) == KERN_SUCCESS) {
                io_service_t child;
                while ((child = IOIteratorNext(childIter))) {
                    if (IOObjectConformsTo(child, "IOUSBHostInterface")) {
                        intf = [[IOUSBHostInterface alloc]
                            initWithIOService:child
                                      options:IOUSBHostObjectInitOptionsNone
                                        queue:q
                                        error:&err
                              interestHandler:nil];
                        IOObjectRelease(child);
                        break;
                    }
                    IOObjectRelease(child);
                }
                IOObjectRelease(childIter);
            }
            if (!intf) usleep(20000);
        }
        if (!intf) {
            fprintf(stderr, "interface open failed: %s\n",
                    err ? err.description.UTF8String : "interface nub not found");
            return 1;
        }

        IOUSBHostPipe *pipe = [intf copyPipeWithAddress:ep->bEndpointAddress
                                                  error:&err];
        if (!pipe) {
            fprintf(stderr, "pipe open failed: %s\n", err.description.UTF8String);
            return 1;
        }

        printf("Captured %04lX:%04lX serial %s — detached from the HID stack; "
               "ep=0x%02X maxPacket=%u bInterval=%u\n",
               kVendorID, kProductID, serial.UTF8String, ep->bEndpointAddress,
               maxPacket, ep->bInterval);
        fflush(stdout);

        // Keep an interrupt-IN request always outstanding — re-arm from the
        // completion handler — so the endpoint is polled every bInterval, the
        // way the real HID host does. A one-at-a-time synchronous read leaves
        // the endpoint unpolled between transfers and the firmware's
        // send_report stalls (~200ms/report observed).
        NSMutableData *buf = [NSMutableData dataWithLength:maxPacket];
        __block uint64_t prevMonoNs = 0;
        __block void (^armRead)(void);
        IOUSBHostCompletionHandler onReport = ^(IOReturn status, NSUInteger transferred) {
            if (status != kIOReturnSuccess) {
                fprintf(stderr, "read status 0x%08X\n", status);
                if (status == kIOReturnAborted) return; // shutting down
                armRead();
                return;
            }
            struct timespec wall, mono;
            clock_gettime(CLOCK_REALTIME, &wall);
            clock_gettime(CLOCK_MONOTONIC_RAW, &mono);
            uint64_t monoNs =
                (uint64_t)mono.tv_sec * 1000000000ull + (uint64_t)mono.tv_nsec;
            long dtUs = prevMonoNs ? (long)((monoNs - prevMonoNs) / 1000ull) : -1;
            prevMonoNs = monoNs;

            const uint8_t *b = buf.bytes;
            printf("report ts=%ld.%06ld dt=%ld len=%lu:", (long)wall.tv_sec,
                   (long)(wall.tv_nsec / 1000), dtUs, (unsigned long)transferred);
            for (NSUInteger i = 0; i < transferred; i++) printf(" %02X", b[i]);
            printf("\n");
            fflush(stdout);
            armRead();
        };
        armRead = ^{
            NSError *e = nil;
            buf.length = maxPacket;
            if (![pipe enqueueIORequestWithData:buf
                              completionTimeout:0
                                          error:&e
                              completionHandler:onReport]) {
                fprintf(stderr, "enqueue failed: %s\n", e.description.UTF8String);
            }
        };
        armRead();
        dispatch_main(); // completion handlers run on q; never returns
        return 1;
    }
}
