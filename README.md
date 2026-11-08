# adbee

A friendly companion for `adb`: see your Android devices at a glance, move files, and pair over WiFi with a QR code.

```console
$ adbee devices
● Sam's Pixel  ready
  Google Pixel 8 · 3A091FDJH00ABC
  Android   15 · API 35 · patch 2025-10-05
  Battery   ▰▰▰▰▰▰▰▱▱▱ 65% · charging · 28.0°C
  Storage   49G free of 110G
  Hardware  8 GB RAM · 1080x2400 · arm64-v8a
  Network   192.168.1.20 on "Home"
  Via       wifi (auto-connected by adb)
```

## Install

```bash
cargo install adbee
```

You need [Android platform-tools](https://developer.android.com/tools/releases/platform-tools) (`adb`) on your `PATH`. adbee starts the adb server for you.

## Usage

| Command | What it does |
|---|---|
| `adbee devices` | Each device's name, model, serial, Android version, battery, storage, hardware, IP and WiFi network |
| `adbee push <file\|folder> [dest]` | Copy to the device (default `/sdcard/Download/`), with a progress bar |
| `adbee pull <path> [dest]` | Copy a file or folder from the device (default: current folder) |
| `adbee ls [path]` | List a folder on the device (default `/sdcard`) |
| `adbee screenshot [file]` | Save a screenshot as PNG |
| `adbee` or `adbee pair` | Pair over WiFi by scanning a QR code, then connect |
| `adbee pair --code [ip:port]` | Pair by typing the 6-digit code shown on the phone |
| `adbee connect [ip\|ip:port]` | Reconnect to an already-paired phone over WiFi |
| `adbee disconnect [serial\|ip:port]` | Disconnect a wireless device, or all of them |
| `adbee completion <shell>` | Tab completion for commands, device serials, and file paths on the phone (`--install` to set it up) |

With several devices connected, pick one with `-s <serial>` or `ANDROID_SERIAL`, or choose from the menu adbee shows.

### WiFi pairing

1. Phone and computer on the same network.
2. On the phone: **Developer options → Wireless debugging → Pair device with QR code**.
3. Run `adbee` and scan the code.

No camera handy? Choose **Pair device with pairing code** instead and run `adbee pair --code`. If your network blocks discovery (common on office WiFi), pass the address shown on that screen: `adbee pair --code 192.168.1.20:37099`.

After a reboot the phone's port changes; `adbee connect` finds it again. IPv4 and IPv6 both work.

## Development

```bash
cargo test                          # unit tests, no phone needed
cargo test --test phone -- --ignored   # end-to-end tests against one connected phone (or `mise run test-phone`)
```

The phone tests never pair, connect or disconnect. Files they write go in a fresh `/sdcard/Download/adbee-test-*` folder that is deleted afterwards.

## License

MIT
