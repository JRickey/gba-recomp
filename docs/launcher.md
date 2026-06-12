# gba-launcher: the frontend

A small egui/eframe application for picking a cartridge image, launching
it in the play runtime, and configuring input. Visual language is
deliberately turn-of-the-millennium — indigo translucent plastic, brushed
chrome, glacier glass, glossy capsule buttons, orb bullets, hazard
stripes — keyed off the launch-era hardware's look.

## Asset policy

Every visual is painted procedurally with egui primitives at runtime,
including the window icon (computed pixel-by-pixel at startup). The crate
ships **zero image assets**, so there is nothing to license or attribute
beyond the GUI stack itself. Text renders with egui's bundled
open-licensed default fonts.

## Screens

- **PLAY** — system file dialog, drag-and-drop anywhere in the window,
  and a persisted recently-played shelf (presence orb, hover play
  chevron, forget cross). Launching spawns `recomp play <image>`; the
  binary is resolved from `$GBA_RECOMP_BIN`, then next to the launcher
  executable, then `$PATH`.
- **INPUT** — device pills (keyboard plus every connected pad, live via
  gilrs hot-plug) and click-to-capture rebinding for the ten KEYINPUT
  buttons. Capture takes a key, a pad button, or a stick push, so any GBA
  button can map to a stick direction. On a gamepad an **Analog** section
  selects which source drives the d-pad (left stick / right stick /
  physical d-pad / both) and a deadzone slider; a live monitor lights the
  GBA buttons the selected pad is currently pressing. Saved on every
  change to the shared config (below); `recomp play` reads the same file,
  so bindings apply without re-launching the frontend. Modifier keys
  cannot be captured (egui reports them as modifiers, not keys); the
  default Select=RightShift survives unless rebound.
- **A/V** — placeholder (hazard stripes): scaling, filters, and audio mix
  land here.

## Input config

`<config dir>/gba-recomp/input.cfg` — plain `key = value` lines:
`device`, `gamepad_name`, `dpad_source`, `stick_deadzone`, `key.<button>`,
`pad.<button>` (see the `input-config` crate). Key names are a canonical
set mapped to egui keys on the launcher side and minifb keys on the play
side; pad names are gilrs button names or stick-direction tokens
(`LeftStickUp`, `RightStickRight`, ...). `dpad_source` is
`leftstick|rightstick|dpad|both` (default `both`); `stick_deadzone` is a
float clamped to `[0.05, 0.95]` (default `0.50`). Unknown values fall
back to defaults per-key. A vendored community controller-mapping
database (`assets/gamecontrollerdb.txt`, embedded into both binaries) is
layered over gilrs's built-in mappings for the widest pad compatibility.

## Platform integration

| Platform | File selection | Status |
|---|---|---|
| Linux | XDG desktop portal (via rfd) | works |
| macOS | NSOpenPanel (via rfd) | works |
| Windows | IFileOpenDialog (via rfd) | built, untested on hardware |
| Android | SAF document picker | scaffolding only (below) |

## Android status

The UI itself targets Android through eframe/winit's NativeActivity
backend: the crate builds as a cdylib, `android_main` is provided, and
the `android-native-activity` feature wires the glue:

```
cargo build --target aarch64-linux-android --features android-native-activity
```

This path is **unverified** — it needs the NDK toolchain plus an apk
wrapper (e.g. cargo-apk) and has not run on a device.

File selection on Android must go through the Storage Access Framework
(`ACTION_OPEN_DOCUMENT`); scoped storage forbids raw filesystem browsing.
NativeActivity does not forward `onActivityResult`, so the picked URI
cannot reach Rust through the plain glue: the plan is a small Java
activity subclass that fires the picker and passes the URI across JNI
(then a content-resolver read into a local cache file). Launching also
awaits a play runtime for the target. Both are TODO; the stubs in
`platform.rs` document the same.
