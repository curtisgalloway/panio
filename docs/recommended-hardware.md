# Recommended hardware

## Recommended by Curtis Galloway

These are my personal recommendations; they don't imply any endorsement of any particular hardware or vendor.  It's just what I use.

Your hardware choices depend on your use case.

* **Low level debugging or bringup work**: you will want full control: power cycling, connecting and disconnecting ethernet and USB, viewing video, injecting HID events, serial console, netbooting, attaching and detaching a USB storage device.

* **Building or testing on a stable device**: you may not need to connect and disconnect ethernet and USB, but you probably want at least serial, video, HID, and power cycling.

In my case, I'm doing bringup and driver testing, so I need the whole gamut.  Here are the devices I find most useful for everyday work.

### Video / HID

**First choice: [Sipeed NanoKVM USB](https://github.com/sipeed/NanoKVM-USB)**

The NanoKVM USB is cheap, between $40 and $60 USD depending on where you get it, at the time of this writing.  And it's also very capable.  You can switch a USB device between the host and target, for writing a flash drive and then booting from it.

Runners up:

* Generic USB HDMI capture dongle, if you only need video capture.
* Openterface KVM Go: compact and includes a switchable SD card slot that you can switch between host and target.
* Openterface Mini KVM: a little more capable but the serial control interface has a few quirks.

### Serial

**First choice: [DSD Tech SH-U09C5 USB Serial Adapter](https://www.deshide.com/product-details_SH-U09C5.html)**

Switchable to multiple voltages and breaks out lots of serial signals.  Very reliable.

Runners up:

* any generic USB serial adapter that works with your board's voltage.

### Network

**First choice: [DLink DGS-1100-05V2 5-port managed switch](https://www.dlink.com/us/en/products/dgs-1100-05v2-5-port-gigabit-smart-managed-switch)**

This managed 1GB switch lets you connect a control host to up to four targets for netbooting and other experiments, and gives you full control of the ethernet ports so you can test that the target correctly sees link down/up events.

Runners up:

* [TP-Link TL-SG205E 5-port switch](https://www.tp-link.com/us/business-networking/managed-switch/tl-sg205e/): another managed switch that gives you similar control.

### Power

**First choice: [DSD Tech SH-UR04A 4-Channel USB Powered Relay Module](https://www.deshide.com/product-details_SH-UR04A.html)**

This is a convenient way to power up to four boards with the ability to power-cycle them.  You can source up to about 5A per channel at 5-12V, which is plenty for most SBCs you will want to use.  It has a pretty sensible USB interface that `panio` can drive easily; the only unfortunate part is that they don't have unique serial numbers, so if you have more than one attached you'll have to locate them by the attached port.

Runners up:

* [DFRobot Gravity Easy Relay Module](https://www.dfrobot.com/product-2094.html): has 5mm barrel jack input and output with GPIO control; use with a DSD Tech USB Serial adapter for easy control using the DTR# signal.
* [Shellyplug multi-protocol power switch](https://us.shelly.com/products/shelly-plug-us-gen4-white): Supports Wifi, Bluetooth or ZigBee; inexpensive and flexible.
* Generic USB Zigbee dongle and Zigbee power switch (or your existing HomeAssistant setup with a Zigbee power switch).

### USB hub with ethernet port

First choice: **[UGREEN USB to Ethernet Adapter](https://www.amazon.com/UGREEN-Port-Gigabit-Ethernet-Adapter/dp/B0956BLF4S)**

Has both USB-C and USB-A ports and is inexpensive.  They make a 10GB model as well.

Runners up:

* TP-Link USB 3.0 to Ethernet Adapter (UE330).
* Anker Unibody Aluminum 3-Port USB 3.0 Ethernet Hub.
* Any generic hub that works with your control host.