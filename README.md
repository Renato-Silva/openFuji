# openFuji

Control Fujifilm cameras over Wi-Fi from Linux: remote shooting with live view,
exposure settings, and browsing/downloading the pictures on the card.

It is a Rust reimplementation of the camera protocol used by the
*FUJIFILM Camera Remote* Android app (4.10.2). It talks to the camera directly;
nothing from the app is bundled.

## Programs

| Binary          | What it is |
|-----------------|------------|
| `openfuji`      | Desktop app (egui): live view, shutter, settings, picture gallery and download |
| `openfuji-cli`  | Command-line tool for scripting and protocol debugging |
| `openfuji-mock` | A fake camera on localhost, for trying things without hardware |

```sh
cargo build --release
./target/release/openfuji                 # GUI
./target/release/openfuji 192.168.0.1     # GUI, connect immediately (add --gallery for the picture view)
```

## Connecting

**Bluetooth cameras (X-E3, X-T3 and newer).** These only serve Wi-Fi clients that were introduced over Bluetooth.
1. Pair once: on the camera choose *Bluetooth settings → Pairing registration*, then click **Pair camera…**
   (or run `openfuji-cli ble-pair`). The pairing is saved in `~/.config/openfuji/pairings`.
2. Tick **Wake Wi-Fi via Bluetooth** and click **Connect**. openFuji asks the camera to start its Wi-Fi,
   joins it with NetworkManager, and switches back to your previous Wi-Fi when you disconnect.

**Wi-Fi-only cameras (X-T2, X-Pro2, …).** Start the wireless connection on the camera, join its
`FUJIFILM-…` network, untick Bluetooth and connect to `192.168.0.1`.

*Remote shooting* and *Pictures on camera* are separate camera functions, so
switching tabs reconnects.

## CLI

```sh
openfuji-cli info                              # model, firmware, operations, Fuji state
openfuji-cli props                             # all property descriptors
openfuji-cli ls                                # pictures on the card
openfuji-cli download --latest 10 -o ~/Pictures/fuji
openfuji-cli liveview --stdout | ffplay -f mjpeg -i -
openfuji-cli shoot --af
openfuji-cli raw 1001                          # any PTP operation, hex dump
openfuji-cli -vv info                          # log every packet in hex
```

## Trying it without a camera

```sh
openfuji-mock 127.0.0.1 &      # serves 12 generated pictures and an animated live view
openfuji 127.0.0.1
cargo test                     # includes an end-to-end session against the mock
```

## Protocol notes

Recovered from `libFTLPTPIP.so`, `libFFIR.so` and the Java layer of the app.

- **Ports:** 55740 commands, 55741 events, 55742 live view. When the camera is the access point, it is at `192.168.0.1`.
- **Handshake:** an 82-byte `Init_Command_Request`:
  `u32 len=0x52, u32 type=1, u32 0x8f53e4f2, 16-byte GUID, client name UTF-16LE (54 bytes)`.
  The app's GUID is `ada5485d87b27f0bd3d5ded0` followed by the phone's IPv4 address.
  The reply is either a 68-byte ack (type 2: connection number, camera GUID, camera name) or a 12-byte
  `Init_Fail` (type 5) with a reason code. The app retries `0x2019` (busy) up to 6 times, 500 ms apart.
- **After the handshake,** packets are plain USB-style PTP containers over TCP:
  `u32 length, u16 type (1 cmd / 2 data / 3 resp / 4 event), u16 code, u32 transaction id, payload`.
  The transaction id resets to 0 before `OpenSession(1)`. If the reply is `SessionAlreadyOpen`, close and reopen.
- **Function mode:** set `0xDF01` (u16): `5` for remote, `2` for viewer (`9` for the "Ver1.2" viewer).
  Then read the version property and write the app's version: `0xDF24 = 0x2000C` for remote,
  `0xDF22 = 5` for the viewer. Others: `0xDF21` photo receive (4), `0xDF31` GPS (2), `0xDF26` reserved receive (1).
  `0xDF00` holds the camera-side state.
- **Changes:** the app polls `GetDevicePropValue(0xD212)`, which returns `u16 n` followed by `n × (u16 code, u32 value)`.
  `0xD222` is the picture count.
- **Live view:** `InitiateOpenCapture (0x101C)` with 2 params, then connect to 55741 and 55742.
  Each live-view packet is `u32 length` followed by a header and a JPEG. To stop, send
  `TerminateOpenCapture(0x1018, tid of the open capture)`.
- **Shooting:** `InitiateCapture (0x100E)`. Half-press is `0x9026`; release it with `0x9027`.

### Bluetooth LE (from `BTCamera` / `BTManagerService`)

- In pairing mode the camera advertises manufacturer data under company id `0x04D8`. Byte 0 holds flags:
  bit 0 means 4 extra bytes follow; bit 1 means a 4-byte **pairing key** follows (at offset 5 if bit 0 is set, else at offset 1).
- **Pairing:** write the key back to `ABA356EB-9633-4E60-B73F-F52516DBD671`. Then write the client name
  (NUL-terminated) to `85B9163E-…` and the BLE protocol version (u16 `2`) to `EB4166B0-…`.
  The app does not request OS-level bonding.
- **Starting Wi-Fi:** write a u16 function code to `600655E6-3637-42F1-8FB2-44EFC5C63B13`
  (`4` remote, `3` viewer, `1` photo receive, `2` GPS). Then wait for the AP-state notification on
  `A68E3F66-…` to become `1`. Its values are 0 = off, 1 = connectable, 2 = launching, 3 = cancelled.
- **Wi-Fi credentials:** read the SSID from `BF6DC9CF-…` and the passphrase from `E809256A-…`.

## Status

The protocol and the GUI are tested end to end against the bundled mock camera.
They have **not yet been tested against a real camera**. Run `openfuji-cli -vv info`
against your camera to see exactly what goes over the wire, and please report what you find.
Bluetooth pairing and Wi-Fi wake-up are implemented from the app's code, but are also not yet verified on hardware.
