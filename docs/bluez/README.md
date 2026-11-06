# BlueZ patch: pipelined ATT requests

`att-pipelined-requests.patch`, for BlueZ 5.85 (`src/shared/att.c`).

## Why

The R12 ("SR16", RWfit family) acts as a GATT client as soon as it connects: it sends Exchange MTU,
then Find By Type Value (looking for Apple's notification service) one connection event later,
before the phone's MTU response has gone out. That breaks ATT's one-request-at-a-time rule, and
BlueZ answers by closing its ATT channel ("Received request while another is pending: 0x06" in the
debug log). With no ATT channel left, the kernel drops the idle link about 2 s later, logged as
"Connection terminated by local host" (HCI reason `0x13`). Captured on the FLX1s on 2026-11-04 with
`btmon` and bluetoothd debug output. A desktop running BlueZ (2026-11-04) lost the link the same
way about 6 s after connecting, so this is not specific to the phone.

The patch logs the violation and handles the request instead. Responses still leave in the order
the requests came.

## Build and install on FuriOS (Debian)

Needs `deb-src` lines enabled for the FuriOS repositories.

```sh
sudo apt install build-essential devscripts
sudo apt build-dep bluez
apt source bluez
cd bluez-5.85*/
patch -p1 < /path/to/Ringmaster/docs/bluez/att-pipelined-requests.patch
dpkg-buildpackage -us -uc -b
sudo dpkg -i ../bluez_5.85*.deb
sudo apt-mark hold bluez
sudo systemctl restart bluetooth
```

`apt-mark hold` keeps an update from replacing it; `sudo apt-mark unhold bluez` and
`sudo apt install --reinstall bluez` put the distribution's build back.
