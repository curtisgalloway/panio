#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Curtis Galloway
# SPDX-License-Identifier: Apache-2.0
# /// script
# requires-python = ">=3.11"
# dependencies = ["pyusb==1.3.1"]
# ///
"""Controlled Linux USB GET_INFO probes; see notes/openterface-v2-linux-serial.md.

Output is JSON Lines, including UTC and monotonic timestamps. No reset,
SET_CONFIGURATION, SET_INTERFACE, HID injection, or persistent writes are used.
"""

import argparse
from contextlib import contextmanager
from datetime import datetime, timezone
import json
import math
import signal
import struct
import sys
import time

import usb.core
import usb.util

GET_INFO = bytes.fromhex("57 ab 00 01 00 03")


def log(event, **fields):
    """Emit one timestamped record; keep hardware logs out of version control."""
    print(
        json.dumps(
            {
                "utc": datetime.now(timezone.utc).isoformat(),
                "monotonic_ns": time.monotonic_ns(),
                "event": event,
                **fields,
            }
        ),
        flush=True,
    )


def transfer(device, request_type, request, value, index, payload, timeout):
    """Log a control transfer before submission and after completion."""
    log(
        "control_submit",
        request_type=request_type,
        request=request,
        value=value,
        index=index,
        payload=payload.hex() if isinstance(payload, bytes) else payload,
    )
    try:
        result = device.ctrl_transfer(
            request_type, request, value, index, payload, timeout=timeout
        )
    except usb.core.USBError as error:
        log("control_error", error=str(error), errno=error.errno)
        raise
    log(
        "control_complete",
        result=result if isinstance(result, int) else bytes(result).hex(),
    )
    return result


def describe(device):
    """Read descriptors without claiming an interface or selecting a config."""
    configs = []
    for config in device:
        interfaces = []
        for interface in config:
            interfaces.append(
                {
                    "number": interface.bInterfaceNumber,
                    "alternate": interface.bAlternateSetting,
                    "class": interface.bInterfaceClass,
                    "subclass": interface.bInterfaceSubClass,
                    "protocol": interface.bInterfaceProtocol,
                    "extra": bytes(interface.extra_descriptors).hex(),
                    "endpoints": [
                        {
                            "address": endpoint.bEndpointAddress,
                            "attributes": endpoint.bmAttributes,
                            "max_packet_size": endpoint.wMaxPacketSize,
                        }
                        for endpoint in interface
                    ],
                }
            )
        configs.append({"value": config.bConfigurationValue, "interfaces": interfaces})
    log(
        "device",
        bus=device.bus,
        address=device.address,
        ports=list(device.port_numbers or []),
        vid=device.idVendor,
        pid=device.idProduct,
        bcd_device=device.bcdDevice,
        serial_string_index=device.iSerialNumber,
        configurations=configs,
    )


def cdc_pair(config):
    """Find an unambiguous ACM/data pair through its CDC Union descriptor."""
    pairs = []
    for communication in config:
        if (
            communication.bInterfaceClass,
            communication.bInterfaceSubClass,
            communication.bAlternateSetting,
        ) != (2, 2, 0):
            continue
        extra = bytes(communication.extra_descriptors)
        while extra:
            size = extra[0]
            if size < 2 or size > len(extra):
                raise ValueError("Malformed interface descriptor")
            descriptor, extra = extra[:size], extra[size:]
            if descriptor[1:3] != b"\x24\x06":
                continue
            if size != 5 or descriptor[3] != communication.bInterfaceNumber:
                raise ValueError("Unsupported CDC Union; expected one data interface")
            data = config[(descriptor[4], 0)]
            if data.bInterfaceClass != 10:
                raise ValueError("CDC Union does not point at a CDC data interface")
            incoming = [
                ep
                for ep in data
                if ep.bmAttributes & 3 == 2 and ep.bEndpointAddress & 0x80
            ]
            outgoing = [
                ep
                for ep in data
                if ep.bmAttributes & 3 == 2 and not ep.bEndpointAddress & 0x80
            ]
            if len(incoming) != 1 or len(outgoing) != 1:
                raise ValueError("Expected exactly one bulk endpoint in each direction")
            pairs.append((communication, data, outgoing[0], incoming[0]))
    if len(pairs) != 1:
        raise ValueError("Expected exactly one CDC ACM/data pair")
    return pairs[0]


@contextmanager
def claimed(device, interfaces, detach):
    """Restore original bindings even after partial detach/claim failure."""
    originally_bound = [i for i in interfaces if device.is_kernel_driver_active(i)]
    if originally_bound and not detach:
        raise ValueError("Kernel driver bound; stop port users, then use --detach")
    acquired = []
    cleanup_errors = []
    try:
        for interface in interfaces:
            if device.is_kernel_driver_active(interface):
                log("detach", interface=interface)
                device.detach_kernel_driver(interface)
            log("claim", interface=interface)
            usb.util.claim_interface(device, interface)
            acquired.append(interface)
        yield
    finally:
        for interface in reversed(acquired):
            try:
                usb.util.release_interface(device, interface)
                log("release", interface=interface)
            except usb.core.USBError as error:
                cleanup_errors.append(str(error))
                log("release_error", interface=interface, error=str(error))
        for interface in originally_bound:
            try:
                if not device.is_kernel_driver_active(interface):
                    device.attach_kernel_driver(interface)
                log("driver_restored", interface=interface)
            except usb.core.USBError as error:
                cleanup_errors.append(str(error))
                log("restore_error", interface=interface, error=str(error))
        if cleanup_errors:
            raise RuntimeError("Driver cleanup failed; inspect bindings before reuse")


def info_payload(stream):
    """Return a checksum-valid address-zero GET_INFO payload, if present."""
    for offset in range(max(0, len(stream) - 5)):
        if stream[offset : offset + 4] != b"\x57\xab\x00\x81":
            continue
        size = stream[offset + 4]
        frame = stream[offset : offset + size + 6]
        if size >= 3 and len(frame) == size + 6 and sum(frame[:-1]) & 0xFF == frame[-1]:
            return frame[5:-1]
    return None


def get_info(device, outgoing, incoming, timeout):
    """Send exactly one query, collecting split replies until the deadline."""
    log("bulk_out_submit", endpoint=outgoing.bEndpointAddress, hex=GET_INFO.hex())
    written = device.write(outgoing.bEndpointAddress, GET_INFO, timeout=timeout)
    log("bulk_out_complete", bytes_written=written)
    if written != len(GET_INFO):
        raise RuntimeError("Short bulk OUT write")
    deadline = time.monotonic() + timeout / 1000
    received = bytearray()
    while time.monotonic() < deadline:
        remaining = max(1, math.ceil((deadline - time.monotonic()) * 1000))
        log(
            "bulk_in_submit",
            endpoint=incoming.bEndpointAddress,
            size=incoming.wMaxPacketSize,
            timeout_ms=remaining,
        )
        try:
            chunk = device.read(
                incoming.bEndpointAddress, incoming.wMaxPacketSize, timeout=remaining
            )
        except usb.core.USBTimeoutError:
            log("bulk_in_timeout", received_hex=received.hex())
            break
        log("bulk_in_complete", hex=bytes(chunk).hex())
        received.extend(chunk)
        payload = info_payload(received)
        if payload is not None:
            log(
                "get_info_reply",
                payload_hex=payload.hex(),
                chip_version=payload[0],
                target_connected=bool(payload[1]),
            )
            return True
        if len(received) > 65536:
            raise RuntimeError("Unexpected continuous input; stopping probe")
    log("no_valid_get_info_reply", received_hex=received.hex())
    return False


def drain_input(device, incoming, seconds):
    """Record queued/startup bytes before a query, without counting a reply."""
    deadline = time.monotonic() + seconds
    total = 0
    while time.monotonic() < deadline:
        remaining = max(1, math.ceil((deadline - time.monotonic()) * 1000))
        try:
            chunk = device.read(
                incoming.bEndpointAddress, incoming.wMaxPacketSize, timeout=remaining
            )
        except usb.core.USBTimeoutError:
            break
        log("pre_query_input", hex=bytes(chunk).hex())
        total += len(chunk)
        if total > 65536:
            raise RuntimeError("Unexpected continuous input; stopping drain")
    log("drain_complete", bytes_read=total)


def parse_step(value):
    """Validate every requested step before touching USB."""
    if value == "info":
        return ("info", None)
    name, separator, raw = value.partition("=")
    try:
        number = float(raw) if name in ("wait", "drain") else int(raw)
    except ValueError as error:
        raise argparse.ArgumentTypeError(
            "Expected info, baud=N, lines=N, wait=S, drain=S"
        ) from error
    if separator and (
        (name == "baud" and number in (9600, 115200))
        or (name == "lines" and number in range(4))
        or (name == "wait" and math.isfinite(number) and 0 <= number <= 30)
        or (name == "drain" and math.isfinite(number) and 0 < number <= 30)
    ):
        return (name, number)
    raise argparse.ArgumentTypeError(
        "Use baud=9600/115200, lines=0..3, wait=0..30, drain=S (0 < S <= 30), or info"
    )


def run_probe(device, args):
    """Run one ordered trial without silently configuring the device."""
    config = device.get_active_configuration()
    communication, data, outgoing, incoming = cdc_pair(config)
    interfaces = [communication.bInterfaceNumber, data.bInterfaceNumber]
    log(
        "trial",
        configuration=config.bConfigurationValue,
        communication_interface=interfaces[0],
        data_interface=interfaces[1],
        out_endpoint=outgoing.bEndpointAddress,
        in_endpoint=incoming.bEndpointAddress,
        steps=args.step,
        kernel_initialization_already_possible=True,
    )
    success = True
    with claimed(device, interfaces, args.detach):
        for interface in interfaces:
            alternate = transfer(device, 0x81, 0x0A, 0, interface, 1, args.timeout_ms)
            if bytes(alternate) != b"\x00":
                raise ValueError("Nonzero alternate setting; refusing to change it")
        for name, value in args.step:
            log("step", name=name, value=value)
            if name == "wait":
                time.sleep(value)
            elif name == "drain":
                drain_input(device, incoming, value)
            elif name == "info":
                success = (
                    get_info(device, outgoing, incoming, args.timeout_ms) and success
                )
            else:
                payload = (
                    struct.pack("<IBBB", value, 0, 0, 8) if name == "baud" else b""
                )
                written = transfer(
                    device,
                    0x21,
                    0x20 if name == "baud" else 0x22,
                    0 if name == "baud" else value,
                    interfaces[0],
                    payload,
                    args.timeout_ms,
                )
                if written != len(payload):
                    raise RuntimeError("Short CDC control transfer")
    return 0 if success else 1


def main():
    """Parse arguments; enumerate only, or run an explicitly selected trial."""
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("list", help="Read descriptors of all 1a86:fe0c devices")
    probe = commands.add_parser(
        "probe", help="Send an ordered information-only USB trial"
    )
    probe.add_argument("--bus", type=int, required=True, help="Current USB bus number")
    probe.add_argument(
        "--address", type=int, required=True, help="Current USB device address"
    )
    probe.add_argument(
        "--detach",
        action="store_true",
        help="Temporarily detach bound drivers; stop all port users first",
    )
    probe.add_argument(
        "--timeout-ms", type=int, default=2000, help="Transfer timeout (1–30000)"
    )
    probe.add_argument(
        "--step",
        type=parse_step,
        action="append",
        required=True,
        help=(
            "Ordered step: info, baud=9600/115200, lines=0..3 "
            "(DTR bit 0, RTS bit 1), wait=seconds, drain=seconds "
            "(record/discard queued input before a query)"
        ),
    )
    args = parser.parse_args()
    if args.command == "probe":
        if not 1 <= args.timeout_ms <= 30000:
            parser.error("--timeout-ms must be 1–30000")
        if not any(name == "info" for name, _ in args.step):
            parser.error("Include at least one --step info")
        if sys.platform != "linux":
            parser.error("Direct probing is supported only on Linux")
    devices = list(usb.core.find(find_all=True, idVendor=0x1A86, idProduct=0xFE0C))
    try:
        if args.command == "list":
            for device in devices:
                describe(device)
            log("enumeration_complete", count=len(devices))
            return 0
        matches = [d for d in devices if (d.bus, d.address) == (args.bus, args.address)]
        if len(matches) != 1:
            raise ValueError(
                "Selection does not match exactly one 1a86:fe0c device; list again"
            )
        describe(matches[0])
        return run_probe(matches[0], args)
    finally:
        for device in devices:
            usb.util.dispose_resources(device)


def interrupted(signum, frame):
    """Make SIGTERM follow the same cleanup path as Ctrl-C."""
    del signum, frame
    raise KeyboardInterrupt


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, interrupted)
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        log("interrupted")
        sys.exit(130)
    except (usb.core.USBError, ValueError, RuntimeError) as exc:
        log("error", error=str(exc))
        sys.exit(2)
