# iOS capture and control strategy

iOS is intentionally harder to mirror and control than Android: Apple provides
no public equivalent of ADB input injection, and screen capture of arbitrary
apps is restricted. FluxBridge takes a realistic, two-track approach that stays
within the limits of the Apple ecosystem. This document analyzes the options for
**video capture** and **control**, and states the chosen direction.

## Video capture options

### Option A: internal AirPlay mirroring server + mDNS + RTP H.264

Run a small AirPlay receiver on the PC. The iPhone's built-in Screen Mirroring
(Control Center) discovers the receiver via mDNS / Bonjour (`_airplay._tcp`,
`_raop._tcp`) and streams the whole screen as H.264 over RTP, with audio.

- Pros: uses Apple's own, user-initiated mirroring; no jailbreak; no app install;
  whole-screen capture at good quality and latency; works wirelessly.
- Cons: AirPlay handshake includes proprietary pairing/encryption that must be
  re-implemented carefully; capture is full-screen only (no per-app isolation);
  version drift between iOS releases can break the receiver.

### Option B: libimobiledevice screen capture

Use the developer screen-capture service over USB (the mechanism Xcode/QuickTime
use via the CoreMediaIO device).

- Pros: wired, low-latency, no AirPlay re-implementation.
- Cons: depends on the device being trusted and, in practice, on developer
  tooling; brittle across iOS versions; USB only.

## Control options

### Option C: WebDriverAgent (WDA)

Facebook/Appium's WebDriverAgent runs an on-device HTTP server exposing the
XCTest automation API (taps, swipes, text, hardware buttons).

- Pros: the only realistic way to synthesize input on iOS; well-understood via
  Appium.
- Cons: requires building and deploying a signed WDA app (developer account or
  provisioning); sessions expire; not a consumer-grade install path; higher
  input latency than Android's `InputManager`.

### Option D: libimobiledevice for device services

libimobiledevice speaks the lockdown/usbmux protocols for app install,
diagnostics, file transfer (AFC) and launching WDA. It does not itself inject
input, but it is the plumbing that makes WDA deployable and enables file and
clipboard flows over USB.

## Chosen direction

FluxBridge models iOS behind the same core traits as Android
(`VideoCapture`, `InputSink`, `ClipboardSync`) and ships two cooperating
backends:

- **Capture:** primary path is the internal **AirPlay mirroring server + mDNS +
  RTP H.264** (Option A) because it needs no app install and works wirelessly
  with the user's own screen-mirroring gesture. A **libimobiledevice** USB path
  (Option B) is a fallback for wired, lower-latency capture where available.
- **Control:** **WebDriverAgent** (Option C) provides input injection, deployed
  and supervised over USB via **libimobiledevice** (Option D). Control is
  therefore best-effort and clearly surfaced to the user as "mirroring only" when
  WDA is not provisioned.

This keeps iOS support honest about Apple's constraints while reusing the exact
same protocol and trait surface as the Android pipeline. None of these
integrations are linked into `fluxbridge-core`; they are separate backends
behind the core traits, so the portable crate still builds everywhere.
