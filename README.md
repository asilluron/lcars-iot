# Home panel

A native Rust panel for a wall screen. It draws an original control surface with [Slint](https://slint.dev), reads house state from NATS JetStream KV, and sends absolute commands over request/reply. The full design is in [docs/technical-specification.md](docs/technical-specification.md).

The screen binary is `lcars-app`. On a Raspberry Pi 5 it renders with the LinuxKMS FemtoVG backend and no libseat (`backend-linuxkms` plus `backend-linuxkms-libinput`, which is the current form of the spec's `backend-linuxkms-noseat` feature). On a laptop the same binary uses the winit backend.

## Workspace

| Crate | Role |
| --- | --- |
| `home-proto` | `home.v1` Protobuf types (`prost`) |
| `lcars-sync` | NATS connection, KV watch, command tracker. No UI dependency |
| `lcars-ui` | Slint components and theme tokens |
| `lcars-app` | Binary: config, wiring, heartbeat file, display power |

`proto/home/v1/home.proto` is the schema shared with the Go backend. `buf lint` and `buf breaking` cover it.

## Desktop

On macOS the same binary opens a 1024×600 window with the winit backend:

```bash
cargo run -p lcars-app
```

The clock is Mountain time (`America/Denver`) until a calendar reply includes the server's timezone. The calendar home is drawn locally until `NATS_URL` is set. With that set, the screen requests `lcars.calendar.get` from lcars-api and refreshes when `lcars.calendar.updated` arrives. The to-do column stays the sample list until `lcars.reminders.list` answers; a tap then marks that reminder done. A `tls://` URL also needs `NATS_CA_B64` (base64 PEM) or `NATS_CA_FILE` (path to the PEM). The orange status pill shows `LINKING`, `SYNCED`, `NO LINK`, or `NO CALENDAR`. Every control brightens while pressed and plays a short cue from `assets/sounds`. `AUDIO_ENABLED=0` mutes the cues.

Security keeps its lock, home, and away controls. Until `NATS_URL` is set, the two cards under the title are local stand-ins, Front door and Garage. With `NATS_URL`, the screen requests `lcars.cameras.list` and replaces those cards with the cameras lcars-api reports. Stills and the live picture need `LCARS_HTTP_URL`, the origin of that same server, for example `http://192.0.2.10:8080`. The screen opens the `snapshot_path` and `stream_path` from the catalog on that origin. When lcars-api requires a device token, set both `LCARS_DEVICE_USER` and `LCARS_DEVICE_SECRET`. The screen sends that pair as the NATS username and password, and as HTTP basic auth for stills and live video. The live view decodes Annex-B with FFmpeg 7, so a build needs `libavcodec`, `libavformat`, `libavutil`, and `libswscale`.

The Pi 5 image uses this same binary with `SLINT_BACKEND=linuxkms-femtovg` on the 1024×600 panel. Build it with `balena push <fleet>` from the repo root.

## Balena

`docker-compose.yml` is the fleet service. Build context is the repo root so the Dockerfile in `lcars/` can see the workspace.

```bash
balena push <fleet>
```

Device variables (`SCREEN_ID`, `NATS_URL`, `NATS_CA_B64`, `NATS_CREDS_B64`, `LCARS_HTTP_URL`, `LCARS_DEVICE_USER`, `LCARS_DEVICE_SECRET`, and the rest of the spec's bootstrap table) are read once at start. For `NATS_URL=tls://…`, set `NATS_CA_B64` to the base64 of the server's `ca.pem`. Credentials are written to tmpfs under `/run/lcars` and are not baked into the image. `SLINT_BACKEND=linuxkms-femtovg` is set on the service.

Button cues are copied to `/opt/lcars/assets`, decoded at startup, and played from memory through an output stream that stays open. The stream asks for a 5 ms period, so a press does not wait on a new player process. On the Pi the stream is ALSA via `/dev/snd`. A USB audio device on the panel is used when one is present; otherwise playback goes to the connected HDMI port. `AUDIO_DEVICE` overrides that choice, for example `plughw:0,0`.

Wi-Fi power save is already off in balenaOS. The host rule `/usr/lib/udev/rules.d/79-wlan-power.rules` runs `iw dev $name set power_save off` for `wlan*` and `mlan*` when the interface appears, and NetworkManager's default is to leave that setting alone. The Pi 5 onboard interface is `wlan0`, so the fleet compose file has nothing further to set. On a device, `iw dev wlan0 get power_save` should report `Power save: off`.

## Checks

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
buf lint
```

M0 (a Pi 5, the chosen panel, and a 60 fps KMS spike) still gates the hardware path. This tree is the protocol, sync rules, component shell, and image layout that work sits on.
