# LCARS Touchscreen UI: Technical Specification

Oct 21, 2026

## Overview

A native Rust UI built with Slint renders an LCARS-style home control panel directly to the display on a Raspberry Pi 5. It reads all state from NATS JetStream KV, sends commands over NATS request/reply, and ships as a balena fleet so every screen updates from one release.

### Goals

| Goal | Target |
| --- | --- |
| Tap to visual feedback | 1 frame (16.7 ms), applied optimistically on device |
| Tap to confirmed state, wired LAN | p95 50 ms or less |
| Container start to first frame | 500 ms or less, independent of network |
| Idle footprint | 2% of one core or less, 80 MB RSS or less |
| Broker or backend outage | UI stays up, shows degraded status, resyncs within 5 s of recovery |
| Fleet updates | One release updates every screen; per-screen layout without a rebuild |

### Non-goals

- Device integrations (Zigbee, Z-Wave, Wi-Fi devices). The backend owns them; the UI only sees entities.
- Automations or rules on the device. The UI displays and commands; logic lives in the backend.
- Web or mobile clients in v1. The protocol allows them later.
- On-device user accounts. Sensitive actions are gated by a PIN the backend verifies.
- Show-derived assets. Artwork, fonts, and sounds are original or openly licensed.

## Hardware and platform

The target is a Raspberry Pi 5 on balenaOS (device type `raspberrypi5`, aarch64), wired Ethernet, and one touch display driven through KMS by the V3D GPU.

| Component | Choice | Notes |
| --- | --- | --- |
| SBC | Raspberry Pi 5, 4 GB | UI needs under 100 MB; 8 GB adds nothing here |
| GPU | VideoCore VII (V3D 7.1), Mesa `v3d` driver, GLES 3.1 | Upstream Pi 5 support landed in [Mesa 23.3](https://www.phoronix.com/news/Mesa-23.3-Released); Debian bookworm ships 22.3.6, so the container must use Debian trixie or newer |
| Display | Open: official Touch Display 2 (DSI) or an HDMI touch panel | DSI on balena has known bring-up pain; HDMI is the lower-risk first target |
| Touch | Capacitive multitouch via libinput | USB HID for HDMI panels, I2C for DSI |
| Network | Gigabit Ethernet | Wi-Fi allowed only with power save disabled; latency targets relax 2x |
| Audio | HDMI audio, USB audio, or I2S DAC HAT | Pi 5 has no analog headphone jack |
| Storage | A2-rated microSD, or NVMe via M.2 HAT | NVMe cuts boot time and improves write endurance |
| Power | Official 27 W USB-C supply (5 V, 5 A) | Lower-rated supplies throttle USB power to the touch panel |
| Cooling | Active cooler | Required in a closed wall enclosure |
| Host OS | balenaOS, latest stable at build time, pinned per fleet | Host config (overlays, rotation) set from the balenaCloud dashboard |

## System architecture

Each screen runs one container that holds a single TLS connection to NATS. Everything it shows comes from KV, and everything it changes goes through the backend.

&#91;embedded content: system architecture · screen, backend host, balenaCloud\]

The backend is the only writer of state, and balenaCloud manages releases and variables but never sits in the data path, so a balenaCloud outage does not affect the house.

## UI stack

The UI is Rust (stable, pinned with `rust-toolchain.toml`) on Slint 1.x with the LinuxKMS backend, which renders straight to the display with no X11 or Wayland. The [Slint LinuxKMS docs](https://docs.slint.dev/latest/docs/slint/guide/backends-and-renderers/backend_linuxkms/) still label this backend experimental, so M0 proves it on the chosen display before any UI work.

### Slint configuration

| Setting | Value | Why |
| --- | --- | --- |
| Cargo feature | `backend-linuxkms-noseat` | No libseat in the container; the app runs as root with direct DRM and input access |
| Renderer | `renderer-femtovg` (GLES), fallback `renderer-skia` | FemtoVG is light and covers flat shapes and text well |
| `SLINT_BACKEND` | `linuxkms-femtovg` | Selects backend and renderer at runtime; `linuxkms-software` is the emergency fallback |
| `SLINT_KMS_ROTATION` | 0, 90, 180, or 270 | Portrait or flipped mounts without touching host config |
| `SLINT_DRM_OUTPUT` | Unset unless two connectors are present | Pins the output if Slint picks the wrong one |
| Desktop dev | `winit` backend, same binary | Fast iteration on a laptop against a dev NATS server |

### Threading model

1. The Slint event loop owns the main thread and renders only when state changes, so idle frames cost nothing.
2. A Tokio runtime on 2 worker threads runs the NATS client, KV watches, command requests, and timers.
3. Sync to UI: the sync task coalesces updates and pushes at most one batch per frame (16 ms) with `slint::invoke_from_event_loop`. This prevents a 500-entity snapshot from flooding the event loop.
4. UI to sync: callbacks send `UiIntent` messages over a bounded `tokio::sync::mpsc` channel (capacity 256). The UI thread never awaits the network.
5. Audio and backlight run on their own small tasks so a slow sysfs write never blocks a frame.

### Workspace layout

| Crate | Responsibility | Key dependencies |
| --- | --- | --- |
| `home-proto` | Generated Protobuf types shared with the Go backend | `prost`, `prost-build` |
| `lcars-sync` | NATS connection, KV watch, state store, command service; no UI dependency | `async-nats`, `tokio`, `tracing` |
| `lcars-ui` | `.slint` component library, theme tokens, panels | `slint`, `slint-build` |
| `lcars-app` | Binary: config, wiring, audio, backlight, health file | `rodio` or `alsa`, `toml`, `tracing-subscriber` |

Keeping `lcars-sync` free of Slint lets it be tested headless in CI against a real NATS server.

## NATS data plane

The backend is the only writer of state. Screens read state from JetStream KV, ask for changes with request/reply on `cmd.>`, and receive alerts on `evt.>`. A screen never writes KV directly.

| Subject or key | Mechanism | Direction | Payload |
| --- | --- | --- | --- |
| KV `home_state`, key `<domain>.<id>` (e.g. `light.kitchen`) | KV watch | Backend to UI | `EntityState` |
| `cmd.<domain>.<id>` | Request/reply, 1000 ms timeout | UI to backend | `Command`, reply `CommandResult` |
| `evt.<kind>.<source>` (e.g. `evt.doorbell.front`) | JetStream stream `EVENTS`, ordered consumer | Backend to UI | `Event` |
| KV `ui_config`, key `screen.<screen_id>` | KV watch | Admin to UI | `ScreenLayout` |
| KV `home_state`, key `sys.backend` | KV watch, written every 5 s | Backend to UI | `BackendHeartbeat` |
| `ui.<screen_id>.heartbeat` | Core publish every 10 s | UI to backend | `ScreenHeartbeat` |
| `ui.<screen_id>.error` | Core publish on fault | UI to backend | `ScreenError` |

### Bucket and stream settings

| Resource | History | Storage | Limits |
| --- | --- | --- | --- |
| KV `home_state` | 1 (5 while debugging) | File | Max value 64 KB |
| KV `ui_config` | 5, for rollback of layouts | File | Max value 256 KB |
| Stream `EVENTS` | All messages | File | Max age 24 h, max 100k messages |

Replicas are 1 on a single server and 3 if the backend moves to a 3-node cluster.

### Rules

- Commands are absolute and idempotent ("set brightness to 80", never "increase by 10"), so a retry is always safe.
- Every command carries a UUIDv7 `command_id`; the backend deduplicates within a 10 s window.
- The backend replies only after the device acknowledges, then writes the new state to KV. The KV write is the source of truth; the reply only closes the request.
- Safety-relevant events (smoke, leak, alarm) go through the `EVENTS` stream so a reconnecting screen still sees them. Transient events (doorbell chime) may use core NATS.
- Payloads are Protobuf with a NATS header `Content-Type: application/x-protobuf; msg=home.v1.<Type>` for debuggability.

## Message schemas

All messages live in one `home.v1` Protobuf package, in a repo directory shared by the Go backend (`protoc-gen-go`) and the Rust UI (`prost-build`). `buf lint` and `buf breaking` run in CI on every change.

```proto
syntax = "proto3";
package home.v1;

// ---- State (KV home_state) ----
message EntityState {
  string id = 1;               // "light.kitchen"
  string name = 2;             // display name
  string area = 3;             // "kitchen"
  bool available = 4;
  int64 updated_unix_ms = 5;
  oneof state {
    LightState light = 10;
    OnOffState on_off = 11;
    ClimateState climate = 12;
    SensorState sensor = 13;
    LockState lock = 14;
    CoverState cover = 15;
    SceneState scene = 16;
  }
}

message LightState {
  bool on = 1;
  uint32 brightness_pct = 2;         // 0-100
  optional uint32 color_temp_k = 3;
  optional Rgb rgb = 4;
}
message Rgb { uint32 r = 1; uint32 g = 2; uint32 b = 3; }
message OnOffState { bool on = 1; }
message ClimateState {
  double current_temp = 1;
  double target_temp = 2;
  TempUnit unit = 3;
  HvacMode mode = 4;
  HvacAction action = 5;             // what it is doing now
}
enum TempUnit { TEMP_UNIT_UNSPECIFIED = 0; FAHRENHEIT = 1; CELSIUS = 2; }
enum HvacMode { HVAC_MODE_UNSPECIFIED = 0; OFF = 1; HEAT = 2; COOL = 3; AUTO = 4; }
enum HvacAction { HVAC_ACTION_UNSPECIFIED = 0; IDLE = 1; HEATING = 2; COOLING = 3; }
message SensorState { double value = 1; string unit = 2; }
message LockState {
  enum State { STATE_UNSPECIFIED = 0; LOCKED = 1; UNLOCKED = 2; JAMMED = 3; }
  State state = 1;
}
message CoverState { uint32 position_pct = 1; }
message SceneState { int64 last_activated_unix_ms = 1; }

// ---- Commands (cmd.<domain>.<id>) ----
message Command {
  string command_id = 1;             // UUIDv7, dedupe key
  string entity_id = 2;
  string screen_id = 3;
  optional string pin = 4;           // only for gated actions, verified by backend
  oneof action {
    SetOnOff set_on_off = 10;
    SetBrightness set_brightness = 11;
    SetColorTemp set_color_temp = 12;
    SetTargetTemp set_target_temp = 13;
    SetHvacMode set_hvac_mode = 14;
    SetLock set_lock = 15;
    SetCoverPosition set_cover_position = 16;
    ActivateScene activate_scene = 17;
  }
}
message SetOnOff { bool on = 1; }
message SetBrightness { uint32 pct = 1; }
message SetColorTemp { uint32 kelvin = 1; }
message SetTargetTemp { double value = 1; TempUnit unit = 2; }
message SetHvacMode { HvacMode mode = 1; }
message SetLock { bool locked = 1; }
message SetCoverPosition { uint32 pct = 1; }
message ActivateScene {}

message CommandResult {
  enum Status {
    STATUS_UNSPECIFIED = 0; OK = 1; REJECTED = 2;
    DEVICE_UNAVAILABLE = 3; DEVICE_TIMEOUT = 4; PIN_REQUIRED = 5; PIN_INVALID = 6;
  }
  string command_id = 1;
  Status status = 2;
  string message = 3;                // shown on screen when not OK
}

// ---- Events (evt.<kind>.<source>) ----
message Event {
  enum Severity { SEVERITY_UNSPECIFIED = 0; INFO = 1; WARNING = 2; CRITICAL = 3; }
  string id = 1;
  string kind = 2;                   // "doorbell", "leak", "smoke"
  string source = 3;
  Severity severity = 4;             // CRITICAL triggers the full-frame red alert
  string title = 5;
  string body = 6;
  int64 unix_ms = 7;
}

// ---- Layout (KV ui_config) ----
message ScreenLayout {
  uint32 schema_version = 1;
  repeated Panel panels = 2;
  string home_panel_id = 3;
}
message Panel {
  string id = 1;
  string title = 2;                  // sidebar button label
  PanelKind kind = 3;
  repeated string entity_ids = 4;
  string accent = 5;                 // theme color token name
}
enum PanelKind {
  PANEL_KIND_UNSPECIFIED = 0; LIGHTS = 1; CLIMATE = 2; SECURITY = 3;
  SENSORS = 4; SCENES = 5; ALERTS = 6;
}

// ---- Health ----
message BackendHeartbeat { string version = 1; int64 unix_ms = 2; }
message ScreenHeartbeat {
  string screen_id = 1;
  string version = 2;
  uint64 uptime_s = 3;
  bool connected = 4;
  float frame_ms_p99 = 5;
  float cmd_latency_ms_p95 = 6;
  float cpu_pct = 7;
  uint32 rss_mb = 8;
}
message ScreenError { string screen_id = 1; string code = 2; string detail = 3; }
```

### Versioning rules

- Within `home.v1`, changes are additive only: new fields, new `oneof` variants, new enum values.
- Removed fields are marked `reserved`; field numbers are never reused.
- An unknown `oneof` variant renders as a generic "UNSUPPORTED" tile instead of crashing, so the backend can ship new device types first.
- A breaking change creates `home.v2` with a parallel bucket (`home_state_v2`) during migration.

## State sync and command flow

The screen draws first and syncs second: the LCARS frame appears from a baked layout in under 500 ms, then fills with live state once the KV snapshot lands. Every tap changes pixels immediately and is reconciled when the authoritative KV update arrives.

### Startup sequence

1. Read bootstrap config from environment variables.
2. Render the frame from the baked default layout with a "CONNECTING" status. No network dependency.
3. Connect to NATS with TLS and per-screen credentials. Retry with exponential backoff from 250 ms to a 5 s cap, plus 20% jitter.
4. Watch `ui_config` for `screen.<screen_id>`. A valid layout replaces the baked one; an invalid one is rejected and reported on `ui.<screen_id>.error`.
5. Watch `home_state` for the keys the layout references. The watch delivers current values first, then live updates.
6. When the initial backlog is drained (pending count reaches 0), flip status to "ONLINE" and enable controls.

### State store rules

- The store is a `HashMap<entity_id, (EntityState, kv_revision)>` inside `lcars-sync`.
- An update applies only if its KV revision is higher than the stored one, so replays after reconnect are harmless.
- On reconnect the watch restarts, re-delivers current values, and the revision check discards anything stale.
- Updates to the UI are coalesced per frame: 200 KV changes in 16 ms become one model update.

### Command flow

1. Tap: the UI applies the new value optimistically and marks the control pending. Feedback lands in the same frame.
2. The UI sends a `UiIntent` to `lcars-sync`, which issues a request on `cmd.<domain>.<id>` with a fresh `command_id`.
3. The backend validates, drives the device, writes the new `EntityState` to KV, then replies.
4. The KV update reaches every screen. On this screen it clears the pending mark and replaces the optimistic value.

Sliders coalesce: at most one command per entity every 100 ms while dragging, last value wins, and the final value always sends on release.

### Outcomes

| Outcome | UI behavior | Timing |
| --- | --- | --- |
| `OK` reply and KV update | Clear pending; KV value is final | Typically 5 to 50 ms wired |
| KV update from another screen during pending | KV wins; pending clears | Immediate |
| `REJECTED`, `DEVICE_UNAVAILABLE`, `DEVICE_TIMEOUT` | Revert to last KV value, red flash, error chirp, message in the status bar | On reply |
| `PIN_REQUIRED` or `PIN_INVALID` | Show PIN pad; resend same `command_id` with PIN | On reply |
| No reply | Retry once with the same `command_id`, then revert and show "NO RESPONSE" | Retry at 1000 ms, revert at 2000 ms |
| Disconnected | Controls disabled; taps are dropped, never queued | While offline |

Commands are dropped while offline on purpose: replaying "unlock front door" 10 minutes late is worse than asking the user to tap again.

The pending mark shows only if confirmation takes over 150 ms, so normal taps never flicker.

## LCARS design system

The look is built from a small Slint component library and one global `Theme` singleton, so every panel composes the same frame, pills, and readouts. All artwork is original; the typeface is Antonio (SIL Open Font License), bundled in the image.

### Components

| Component | What it draws | Notes |
| --- | --- | --- |
| `Frame` | Top bar, left sidebar, the two elbows that join them, content slot | Every screen uses one `Frame`; sidebar holds panel navigation |
| `Elbow` | Curved corner block with outer and inner radius | `Path` element; radii from theme tokens |
| `Pill` | Rounded button, radius = height / 2 | States below; label right aligned, uppercase |
| `BarSegment` | Flat color block in bars and dividers | Used for header bars and filler |
| `Readout` | Label plus value, tabular numerals | No animation on values; changes are instant |
| `SegmentSlider` | Brightness, temperature, position as segmented bars | Coalesced commands while dragging |
| `StatusStrip` | Connection state, clock, last error | Always visible in the top bar |
| `PinPad` | Numeric entry for gated actions | PIN goes to backend, never stored |
| `AlertOverlay` | Full-frame red alert for `CRITICAL` events | Pulses at 1 Hz; dismiss requires a deliberate long press |
| `DiagnosticsOverlay` | FPS, frame time, NATS RTT, KV revision, version | Opened by a 3 s press on the top-left elbow |

### Theme tokens

| Token | Starting value | Use |
| --- | --- | --- |
| `bg` | `#000000` | Background |
| `accent-1` | `#FF9966` | Primary frame and active pills |
| `accent-2` | `#CC99CC` | Secondary pills, sidebar |
| `accent-3` | `#9999FF` | Readouts, sliders |
| `accent-4` | `#FFCC99` | Labels, dividers |
| `alert` | `#CC4444` | Errors, red alert |
| `text-on-accent` | `#000000` | Text on colored blocks |
| `grid` | 8 px | Spacing unit; all sizes are multiples |
| `touch-min` | 48 px | Minimum target, about 9 mm on a 7 inch panel |
| `scale` | 1.0, from config | Scales the whole layout from 1024x600 to 1920x1080 |

### Control states

| State | Visual | Trigger |
| --- | --- | --- |
| Idle | Theme color | Default |
| Pressed | Lighten 25% | Touch down, 0 ms |
| Pending | Blink at 2 Hz | No confirmation after 150 ms |
| Unavailable | 40% opacity, taps show reason | `available = false` |
| Error | Red flash, 300 ms | Command failed |
| Disabled | 25% opacity | Disconnected |

Animations use Slint `animate` at 200 ms or less. Optional decorative number churn in empty bars runs at 2 Hz and can be disabled to hold idle CPU near zero. Shipped branding avoids the LCARS name and show assets; sound effects are original recordings.

## Input, display power, and audio

Touch goes through libinput inside the Slint backend; the app adds idle dimming, wake handling, and low-latency sound on top.

### Touch

- The container needs `/dev/input` and `UDEV=1` so libinput can enumerate the panel at start and on hotplug.
- Rotation uses `SLINT_KMS_ROTATION`. If touch axes do not follow, a udev rule sets `LIBINPUT_CALIBRATION_MATRIX` for the panel.
- Gestures in v1: tap, long press (600 ms), horizontal drag on sliders. No multi-finger gestures.

### Display power

| Phase | Default | Behavior |
| --- | --- | --- |
| Active | Touch within the last 300 s | Full brightness (config, default 80%) |
| Dim | 300 to 360 s idle | 10% brightness |
| Off | Over 360 s idle | Backlight off; rendering paused |
| Night | Config window, e.g. 22:00 to 06:00 | Max brightness 30%, quieter sounds |

- The first touch after Dim or Off only wakes the screen and is not delivered to controls, which prevents accidental commands in the dark.
- A `CRITICAL` event wakes the screen at full brightness regardless of phase. Motion events (`evt.motion.<area>`) can wake it if enabled in config.
- DSI panels: write `/sys/class/backlight/*/brightness`. HDMI panels: render black and attempt DPMS off through the DRM connector property; support varies by panel (open question).

### Audio

- Output: HDMI audio if the panel has speakers, otherwise a USB audio adapter or I2S DAC HAT, since the Pi 5 has no analog jack.
- Sounds are short original WAVs (under 300 ms, 48 kHz, 16-bit), decoded into memory at start.
- Playback through ALSA with a 5 ms period targets under 15 ms tap-to-sound.
- Config: `AUDIO_ENABLED`, `AUDIO_VOLUME`, and quiet hours matching the night window.

## Configuration

Configuration has two tiers: bootstrap settings in balena environment variables, read once at start, and the screen layout in NATS KV `ui_config`, applied live. Fleet-level variables set defaults; device-level variables override them per screen.

### Bootstrap variables

| Variable | Example | Purpose |
| --- | --- | --- |
| `SCREEN_ID` | `kitchen` | Identity, layout key, credential scope; device-level only |
| `NATS_URL` | `tls://192.0.2.10:4222` | Broker address; comma-separated list for a cluster |
| `NATS_CREDS_B64` | base64 of the `.creds` file | Per-screen credentials, written to tmpfs at start |
| `NATS_CA_B64` | base64 of the CA cert | Trust anchor for the private CA |
| `SLINT_BACKEND` | `linuxkms-femtovg` | Renderer selection |
| `SLINT_KMS_ROTATION` | `90` | Mount orientation |
| `UI_SCALE` | `1.0` | Layout scale for the panel resolution |
| `BRIGHTNESS_PCT` | `80` | Active brightness |
| `IDLE_DIM_S` / `IDLE_OFF_S` | `300` / `360` | Display power timers |
| `NIGHT_WINDOW` | `22:00-06:00` | Night brightness and quiet audio |
| `AUDIO_ENABLED` / `AUDIO_VOLUME` | `1` / `60` | Sound effects |
| `LOG_LEVEL` | `info` | `tracing` filter |

### Layout

- Source of truth: `ui_config` key `screen.<SCREEN_ID>`, a `ScreenLayout` message.
- A layout change applies within one frame of the KV update, with no restart and no release.
- Fallback: `default_layout.toml` baked into the image, used before NATS connects and whenever the KV layout fails validation.
- Validation: known panel kinds, at most 12 panels, at most 48 entities per panel, every accent a valid theme token.
- `ui_config` history of 5 gives an instant rollback to a previous layout.

## Balena deployment

The fleet runs one service, `lcars-ui`, built from a two-stage Debian trixie image so Mesa is new enough for the Pi 5 GPU. NATS and the backend run on a separate host and are not part of the fleet.

### docker-compose.yml

```yaml
version: "2.1"
services:
  lcars-ui:
    build: ./lcars
    restart: always
    network_mode: host
    privileged: true                    # M0-M3; replaced in M4 (below)
    labels:
      io.balena.update.strategy: download-then-kill
      io.balena.features.sysfs: "1"     # backlight control
    environment:
      UDEV: "1"
      SLINT_BACKEND: linuxkms-femtovg
    healthcheck:
      test: ["CMD-SHELL", "[ $(( $(date +%s) - $(stat -c %Y /run/lcars/heartbeat) )) -lt 10 ]"]
      interval: 5s
      retries: 2
      start_period: 15s
```

### Dockerfile

```dockerfile
# ---- build ----
FROM rust:1-trixie AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
      pkg-config protobuf-compiler libinput-dev libudev-dev libgbm-dev \
      libdrm-dev libxkbcommon-dev libfontconfig-dev libasound2-dev \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p lcars-app

# ---- runtime ----
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
      libinput10 libudev1 libgbm1 libdrm2 libegl1 libgles2 libegl-mesa0 \
      libgl1-mesa-dri libxkbcommon0 libfontconfig1 libasound2t64 ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/lcars-app /usr/local/bin/lcars-app
COPY assets/ /opt/lcars/assets/
CMD ["/usr/local/bin/lcars-app"]
```

Package names follow trixie's 64-bit time transition (`libasound2t64`); M0 confirms them and checks `eglinfo` reports V3D 7.1 inside the container. The app touches `/run/lcars/heartbeat` from a 1 s Slint timer, so a frozen UI thread fails the healthcheck and the supervisor restarts the container.

### Hardening in M4

Replace `privileged: true` with explicit `devices` for `/dev/dri`, `/dev/input`, and `/dev/snd`. Mapped device directories do not pick up hotplugged nodes, so a panel swap requires a container restart; that is acceptable for a wall mount.

### Release flow

1. v1 builds with `balena push <fleet>` on balena's native arm64 builders. Move to CI (`cargo-chef` layer cache, `balena deploy`) once builds exceed 10 minutes.
2. Production devices are pinned to a release; "track latest" is off.
3. One canary screen tracks the newest release. After 24 h with no crash loops and heartbeat metrics in budget, the fleet is pinned to it.
4. Rollback is re-pinning the previous release, typically under 2 minutes because images are already cached.

### Host configuration

- Display overlays and rotation set through Device Configuration (`BALENA_HOST_CONFIG_*`) in the dashboard, not baked into the image.
- The legacy `gpu_mem` setting does not drive the Pi 5 KMS stack; leave CMA at its default unless M0 shows allocation failures.
- Public device URL off; remote access only over the balena VPN.

## Security

A wall screen is a physical endpoint anyone in the house can touch, so screens get read access to state and request-only access to commands, and the backend decides what each screen may do.

### Transport and identity

- TLS on every NATS connection, server cert from a private CA (`NATS_CA_B64`).
- One NATS user per screen, issued as an NKey/JWT credential with `nsc`. Revoking a screen is a single JWT revocation.
- Credentials arrive via balena device variables and are written to tmpfs (`/run/lcars`), never to the image or disk.

### Per-screen permissions

| Permission | Allowed | Denied |
| --- | --- | --- |
| Publish | `cmd.>`, `ui.<screen_id>.>`, JetStream API calls to create consumers on `KV_home_state`, `KV_ui_config`, `EVENTS` | `$KV.>` (no direct state writes), `evt.>`, `ui.<other>.>` |
| Subscribe | `_INBOX.>`, `$KV.home_state.>`, `$KV.ui_config.screen.<screen_id>`, `evt.>` | Other screens' config keys |

The exact JetStream API subjects are finalized in M1 against the NATS server version in use.

### Command authorization

- The backend checks every `Command` against a policy keyed by `screen_id` and entity, e.g. garage door only from the garage screen.
- Unlocking doors and disarming alarms require a PIN, verified by the backend and rate limited (5 attempts, then 5 minute lockout per screen).
- PINs are never logged, stored on the device, or echoed in `CommandResult`.

### Device and supply chain

- balenaOS production image, SSH only over the balena VPN, public device URL off.
- `cargo audit` and `cargo deny` in CI; builds use `--locked`.
- Rotating a credential is a variable change and a container restart, about 15 s of downtime.

## Reliability and failure modes

The screen never goes blank because something upstream failed: it always renders the frame, says what is wrong in the status strip, and recovers on its own.

| Failure | Detection | UI behavior | Recovery |
| --- | --- | --- | --- |
| NATS unreachable at boot | Connect error | Frame from baked layout, status "NO LINK", controls disabled | Backoff 250 ms to 5 s; full resync on connect |
| Broker drops mid-session | `async-nats` disconnect event | "LINK LOST" after 3 s; values dimmed after 30 s | Auto reconnect; watch re-delivers, revision check discards stale |
| Backend down, NATS up | `sys.backend` older than 15 s | "BACKEND OFFLINE" banner; commands disabled | Clears when heartbeat resumes |
| Single device offline | `available = false` | Tile dimmed; tap shows "UNAVAILABLE" | Clears on next state update |
| Invalid layout pushed | Decode or validation failure | Keeps last good layout | Error on `ui.<screen_id>.error`; fix or roll back `ui_config` |
| UI thread hang | Heartbeat file older than 10 s | (frozen) | Healthcheck fails; supervisor restarts container, under 15 s |
| Panic | Process exit | (blank during restart) | `restart: always`; panic message logged |
| No display connector | DRM init fails | Nothing to show | Log, retry every 5 s; reported on `ui.<screen_id>.error` |
| Power loss | n/a | n/a | Boot to first frame, then normal startup sequence |
| Bad release | Canary crash loop or metrics out of budget | n/a | Fleet stays pinned; canary re-pinned to previous release |

## Performance budgets

Every budget below is measured on the target Pi 5 over wired Ethernet, and a release that misses one does not leave the canary. Wi-Fi targets are 2x the network-bound numbers.

| Metric | Budget | How measured |
| --- | --- | --- |
| Tap to visual response | 16.7 ms (1 frame) | libinput event timestamp vs frame present; spot-check with 240 fps camera |
| Tap to confirmed state | p95 50 ms, p99 150 ms | `command_id` send time vs KV update receipt |
| Remote change to pixels | p95 30 ms | `updated_unix_ms` vs render time (clocks synced by NTP) |
| Container start to first frame | 500 ms | Process start vs first present |
| Snapshot of 500 entities to populated UI | 200 ms | Connect complete vs "ONLINE" |
| Power on to first frame | 25 s, to be confirmed in M0 | Dominated by balenaOS boot and supervisor start |
| Panel transition | 60 fps, 0 dropped frames | Frame time histogram during 200 ms transition |
| Idle CPU | 2% of one core | `ScreenHeartbeat.cpu_pct`, 10 min average |
| Memory | 80 MB RSS | `ScreenHeartbeat.rss_mb` |
| Runtime image | 150 MB compressed | Registry size |
| Sustained load | 50 updates/s across 500 entities, frame p99 under 16.7 ms | Load test with fake backend |

## Observability and testing

Each screen reports its own health over NATS every 10 s, and the sync layer is tested headless against a real NATS server, so most bugs are caught without a Pi.

### Observability

- Logs: `tracing` with structured fields to stdout, visible in balena logs; level from `LOG_LEVEL`.
- Metrics: `ScreenHeartbeat` carries version, uptime, connection state, frame time p99, command latency p95, CPU, and RSS. The backend can aggregate these into a fleet view.
- Errors: layout rejections, DRM failures, and decode errors go to `ui.<screen_id>.error` as well as logs.
- On-device: the diagnostics overlay (3 s press on the top-left elbow) shows live FPS, frame time, NATS RTT, last KV revision, and build version.

### Testing

| Layer | What is tested | How |
| --- | --- | --- |
| State store | Revision ordering, optimistic apply, rollback, coalescing | Pure Rust unit tests in `lcars-sync` |
| Sync integration | Snapshot, live updates, commands, retries, reconnect after server kill | `nats-server -js` in CI plus a fake backend responder |
| Protocol | Lint and backward compatibility | `buf lint`, `buf breaking` against main |
| UI components | Layout and states at 1024x600, 1280x800, 1920x1080 | Slint software renderer screenshots diffed against golden PNGs |
| UI behavior | Tap flows, pending and error states | Slint testing backend, headless |
| Desktop loop | Whole app against dev NATS | Same binary on the `winit` backend |
| Hardware | Display, touch, audio, latency budgets | Canary Pi 5 in the fleet, latency script per release |
| Load | 50 updates/s, 500 entities | Fake backend generator, frame time and CPU from heartbeat |

## Milestones

Work runs in six milestones, and M0 gates everything else because the display and GPU path is the highest-risk unknown.

1. **M0, display spike.** Pi 5 plus the chosen panel on balenaOS, trixie container, a Slint test app on `linuxkms-femtovg`. Exit: `eglinfo` shows V3D 7.1, an animated rectangle holds 60 fps, touch events log correctly, boot time measured.
2. **M1, protocol and sync.** `home.v1` schemas, NATS server with JetStream and both buckets, `lcars-sync` with snapshot, watch, commands, and a fake backend. Exit: integration tests green, including reconnect after a server kill.
3. **M2, LCARS component library.** `Frame`, `Elbow`, `Pill`, `Readout`, `SegmentSlider`, `StatusStrip`, `AlertOverlay`, theme tokens. Exit: golden screenshots at three resolutions, all control states covered.
4. **M3, first panel end to end.** Lights panel against the real backend on the Pi. Exit: tap-to-visual and tap-to-confirmed budgets met.
5. **M4, hardening.** Display power, audio, PIN pad, healthcheck, explicit device mapping, per-screen credentials and permissions. Exit: every row of the failure table exercised on hardware.
6. **M5, fleet.** Remaining panels, release flow with canary pinning, second screen online. Exit: one release updates both screens; layout change applies live.

## Open questions

- [ ] Display: official Touch Display 2 (DSI) or an HDMI panel, and which size and resolution?
- [ ] Orientation: portrait or landscape per mount?
- [ ] Audio output: panel speakers over HDMI, USB adapter, or I2S HAT?
- [ ] NATS topology: single node on the backend host, or a 3-node cluster?
- [ ] Entity count and device domains for v1?
- [ ] Wired Ethernet available at every mount location?
- [ ] Which actions require a PIN beyond locks and alarm disarm?
- [ ] HDMI panels: is DPMS off supported, or is render-black the only option?
- [ ] Storage: microSD or NVMe?

## Sources

- [Slint LinuxKMS backend documentation](https://docs.slint.dev/latest/docs/slint/guide/backends-and-renderers/backend_linuxkms/)
- [Slint cargo features](https://docs.slint.dev/latest/docs/rust/slint/docs/cargo_features/)
- [Mesa V3D driver documentation](https://docs.mesa3d.org/drivers/v3d.html)
- [Mesa 23.3 release with initial Raspberry Pi 5 support (Phoronix)](https://www.phoronix.com/news/Mesa-23.3-Released)
- [balena forum: Pi 5 display issues with the browser block](https://forums.balena.io/t/can-not-get-the-pi-screen-to-work-on-a-pi-5-and-the-browser-block/373868)
