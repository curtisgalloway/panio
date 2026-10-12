# SPDX-FileCopyrightText: 2026 Curtis Galloway
# SPDX-License-Identifier: Apache-2.0
"""Exercise descriptor selection, response validation, and failure cleanup."""

import contextlib
import importlib.util
import io
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import usb.core

SPEC = importlib.util.spec_from_file_location(
    "openterface_probe", Path(__file__).resolve().parents[1] / "openterface_probe.py"
)
probe = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(probe)


class Interface(list):
    """Minimal descriptor collection with PyUSB's public attributes."""

    # PyUSB exposes USB descriptor field names verbatim.
    # pylint: disable=invalid-name

    def __init__(self, number, device_class, endpoints=(), extra=b""):
        super().__init__(endpoints)
        self.bInterfaceNumber = number
        self.bInterfaceClass = device_class
        self.bInterfaceSubClass = 2
        self.bAlternateSetting = 0
        self.extra_descriptors = extra


class Config(list):
    """Implement PyUSB's (interface number, alternate setting) lookup."""

    def __getitem__(self, key):
        if isinstance(key, tuple):
            return next(
                i for i in self if (i.bInterfaceNumber, i.bAlternateSetting) == key
            )
        return super().__getitem__(key)


class ProbeTest(unittest.TestCase):
    """Failure paths must preserve bindings and reject misleading replies."""

    def setUp(self):
        self.enterContext(contextlib.redirect_stdout(io.StringIO()))

    def test_nondefault_endpoints_and_communication_interface(self):
        outgoing = SimpleNamespace(bEndpointAddress=2, bmAttributes=2)
        incoming = SimpleNamespace(bEndpointAddress=0x83, bmAttributes=2)
        communication = Interface(4, 2, extra=bytes([5, 0x24, 6, 4, 7]))
        data = Interface(7, 10, [incoming, outgoing])
        self.assertEqual(
            probe.cdc_pair(Config([data, communication])),
            (communication, data, outgoing, incoming),
        )

    def test_ambiguous_union_refused(self):
        with self.assertRaises(ValueError):
            probe.cdc_pair(
                Config([Interface(0, 2, extra=bytes([6, 0x24, 6, 0, 1, 2]))])
            )

    def test_info_requires_matching_command_address_and_checksum(self):
        prefix = bytes.fromhex("57 ab 00 81 03 02 01 00")
        frame = prefix + bytes([sum(prefix) & 255])
        self.assertEqual(probe.info_payload(b"noise" + frame), b"\x02\x01\x00")
        for corrupted in (
            frame[:-1],
            frame[:-1] + b"\x00",
            frame[:2] + b"\x01" + frame[3:],
            frame[:3] + b"\x82" + frame[4:],
        ):
            self.assertIsNone(probe.info_payload(corrupted))

    def test_split_response_and_exact_query(self):
        device = unittest.mock.Mock()
        prefix = bytes.fromhex("57 ab 00 81 03 02 01 00")
        frame = prefix + bytes([sum(prefix) & 255])
        device.write.return_value = 6
        device.read.side_effect = [frame[:3], frame[3:]]
        self.assertTrue(
            probe.get_info(
                device,
                SimpleNamespace(bEndpointAddress=2),
                SimpleNamespace(bEndpointAddress=0x83, wMaxPacketSize=64),
                2000,
            )
        )
        device.write.assert_called_once_with(
            2, bytes.fromhex("57 ab 00 01 00 03"), timeout=2000
        )

    def test_partial_claim_failure_restores_both_cdc_bindings(self):
        device = unittest.mock.Mock()
        bound = {0, 1}
        device.is_kernel_driver_active.side_effect = lambda i: i in bound
        device.detach_kernel_driver.side_effect = lambda _i: bound.clear()
        device.attach_kernel_driver.side_effect = bound.add
        with patch.object(
            probe.usb.util,
            "claim_interface",
            side_effect=[None, usb.core.USBError("busy")],
        ), patch.object(probe.usb.util, "release_interface") as release:
            with self.assertRaises(usb.core.USBError):
                with probe.claimed(device, [0, 1], True):
                    self.fail("Must not enter probe after failed claim")
            release.assert_called_once_with(device, 0)
        self.assertEqual(bound, {0, 1})

    def test_interrupt_restores_driver(self):
        device = unittest.mock.Mock()
        bound = {0, 1}
        device.is_kernel_driver_active.side_effect = lambda i: i in bound
        device.detach_kernel_driver.side_effect = bound.discard
        device.attach_kernel_driver.side_effect = bound.add
        with patch.object(probe.usb.util, "claim_interface"), patch.object(
            probe.usb.util, "release_interface"
        ) as release:
            with self.assertRaises(KeyboardInterrupt):
                with probe.claimed(device, [0, 1], True):
                    raise KeyboardInterrupt
            self.assertEqual(release.call_count, 2)
        self.assertEqual(bound, {0, 1})

    def test_refuses_detach_without_flag(self):
        device = unittest.mock.Mock()
        device.is_kernel_driver_active.return_value = True
        with self.assertRaises(ValueError):
            with probe.claimed(device, [0, 1], False):
                self.fail("Must not enter probe without detach consent")
        device.detach_kernel_driver.assert_not_called()

    def test_unbound_interfaces_are_not_attached_on_exit(self):
        device = unittest.mock.Mock()
        device.is_kernel_driver_active.return_value = False
        with patch.object(probe.usb.util, "claim_interface"), patch.object(
            probe.usb.util, "release_interface"
        ):
            with probe.claimed(device, [0, 1], False):
                pass
        device.attach_kernel_driver.assert_not_called()

    def test_restore_error_is_failure(self):
        device = unittest.mock.Mock()
        device.is_kernel_driver_active.side_effect = [True, True, False]
        device.attach_kernel_driver.side_effect = usb.core.USBError("disconnected")
        with patch.object(probe.usb.util, "claim_interface"), patch.object(
            probe.usb.util, "release_interface"
        ):
            with self.assertRaises(RuntimeError):
                with probe.claimed(device, [0], True):
                    pass


if __name__ == "__main__":
    unittest.main()
