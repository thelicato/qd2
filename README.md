# QD2

<p align="center">
  <img src="logo.png" alt="QD2 logo" width="220">
</p>

<p align="center">
  <strong>QEMU D-Bus Display</strong><br>
  A modern Rust + GTK4 client for discovering, inspecting, diagnosing, and connecting to QEMU virtual machines exposed through <code>-display dbus</code>.
</p>

<p align="center">
  <code>list</code> • <code>inspect</code> • <code>doctor</code> • <code>connect</code>
</p>

QD2 is built for people who want the flexibility of QEMU's D-Bus display stack without giving up a polished desktop viewer. It combines a CLI that is useful for scripting and debugging with a GTK4 frontend that handles real-world VM workflows like framebuffer rendering, DMABUF scanouts, input grab, clipboard sync, audio playback, reconnects, and diagnostics.

## ✨ Why QD2

- Discover QEMU D-Bus VMs on the session bus and common libvirt private socket locations.
- Inspect consoles, exported interfaces, chardevs, clipboard exposure, and audio exposure.
- Diagnose common host and guest setup problems with `qd2 doctor`.
- Connect to a console with GTK4, keyboard and mouse forwarding, guest cursor updates, fullscreen controls, audio, and clipboard integration.
- Recover more gracefully from disconnects and VM restarts instead of failing silently.
- Tune the viewer with custom hotkeys and targeted runtime diagnostics.

## 🚀 Commands

| Command | Purpose | Example |
| --- | --- | --- |
| `qd2 list` | Enumerate visible QEMU D-Bus VMs. | `cargo run -- list` |
| `qd2 inspect` | Print VM metadata, consoles, chardevs, and exported helper objects. | `cargo run -- inspect --vm demo-vm` |
| `qd2 doctor` | Check the host environment and report likely VM-side wiring issues. | `cargo run -- doctor --vm demo-vm` |
| `qd2 connect` | Open the GTK4 viewer for one console. | `cargo run -- connect --address "unix:path=<path_to_sock>"` |
| `--address <DBUS_ADDRESS>` | Target a specific private D-Bus socket instead of auto-discovery. | `cargo run -- inspect --address "unix:path=<path_to_sock>"` |
| `--verbose` | Print extra discovery and viewer diagnostics. | `cargo run -- --verbose doctor` |
| `--hotkeys ...` | Override viewer shortcuts in a virt-viewer-style format. | `cargo run -- connect --hotkeys "toggle-fullscreen=ctrl+enter,release-cursor=ctrl+alt"` |

## 🖥️ Viewer Highlights

- Software and DMABUF-backed display rendering.
- Keyboard and mouse forwarding with grab and release behavior.
- Guest cursor shape and visibility updates.
- Clipboard sync for text, HTML, URI lists, images, and primary selection support where available.
- Guest audio playback through the QEMU D-Bus audio interface.
- Floating fullscreen controls inspired by virt-viewer.
- Configurable hotkeys for fullscreen, grab release, and DMABUF transforms.
- A VM chooser for the multi-VM `connect` flow.
- Reconnect handling when the listener drops or the VM restarts.

## 🔧 Typical Workflow

```bash
cargo run -- list

cargo run -- inspect --vm demo-vm

cargo run -- doctor --vm demo-vm

cargo run -- connect --vm demo-vm

cargo run -- connect \
  --address "unix:path=<path_to_sock>" \
  --hotkeys "toggle-fullscreen=ctrl+enter,release-cursor=ctrl+alt"

cargo run -- --verbose connect \
  --address "unix:path=<path_to_sock>"
```

## 🧱 Install Requirements

You can either download a prebuilt binary from the [GitHub Releases](https://github.com/thelicato/qd2/releases) page or build QD2 from source.

Building from source currently requires:

- Rust stable with Cargo
- GTK4 development files
- pixman development files
- `pkg-config` or `pkgconf`

Typical package names:

- Debian/Ubuntu: `libgtk-4-dev`, `libpixman-1-dev`, `pkg-config`
- macOS with Homebrew: `gtk4`, `pixman`, `pkgconf`

Build from source with:

```bash
cargo build --release
```

## ⚙️ Runtime Requirements

QD2 expects a QEMU VM exposed through `-display dbus`, either on the session bus or on a private D-Bus socket passed with `--address`.

At runtime, the most important pieces are:

- A QEMU build with D-Bus display support
- A guest with a supported display device and an exported QEMU D-Bus console
- Access to the D-Bus socket you want to connect to
- A working desktop session for GTK4 rendering

Some features have extra requirements:

- Clipboard sync usually needs `-chardev qemu-vdagent,...,clipboard=on` and a `virtserialport` named `com.redhat.spice.0`
- Audio playback works best when QD2 runs inside the same user session as PipeWire or PulseAudio
- DMABUF scanout import is currently Linux-specific and depends on the host GTK stack and GPU/render node support
- Private libvirt sockets often require ACLs or group membership if you want to run QD2 without `sudo`

## 🌍 Platform Notes

- Release artifacts are produced for Linux and macOS on both `x86_64` and `arm64`.
- `qd2 connect` currently targets Unix-style environments.
- DMABUF import is currently available on Linux GTK builds.
- Some host integrations, especially Wayland, PipeWire, and private libvirt sockets, depend on the runtime session and permissions you launch QD2 with.

## ⚠️ Known Limitations

- Linux is the most exercised platform today; macOS builds are produced, but are less battle-tested.
- The viewer is currently Unix-oriented and does not target Windows.
- DMABUF acceleration is Linux-only; other platforms fall back to software framebuffer updates.
- Running QD2 under `sudo` can break desktop integrations like audio or clipboard unless the relevant user-session environment is preserved.
- Some guest features depend on the VM configuration, not just QD2 itself; clipboard and audio both require the right QEMU-side wiring.
- Support is focused on a single interactive viewer window per `connect` session rather than advanced management features like USB redirection or file transfer.

## 📦 Releases

Prebuilt binaries are published on the [GitHub Releases](https://github.com/thelicato/qd2/releases) page.

Each release includes:

- packaged binaries for Linux and macOS on both `x86_64` and `arm64`
- release notes generated with `npx changelogithub`
- a `SHA256SUMS.txt` file for checksum verification

## 🧭 Structure

| File | Purpose |
| --- | --- |
| `cli.rs` | Defines the command-line interface, subcommands, and flags. |
| `diagnostics.rs` | Implements `doctor`, verbose logging, and host-side environment checks. |
| `main.rs` | Wires the CLI to discovery, inspection, diagnostics, and the viewer entry point. |
| `qemu.rs` | Handles QEMU D-Bus discovery, inspection, VM selection, and connection setup. |
| `viewer/mod.rs` | Orchestrates the GTK4 viewer window, event loop, and presentation updates. |
| `viewer/audio.rs` | Registers QEMU audio listeners and forwards guest playback to host audio backends. |
| `viewer/chrome.rs` | Builds the titlebar, fullscreen controls, shortcuts dialog, and about dialog. |
| `viewer/chooser.rs` | Shows the VM selection window when `connect` sees more than one possible target. |
| `viewer/clipboard.rs` | Bridges GTK clipboard state and the QEMU clipboard protocol in both directions. |
| `viewer/cursor.rs` | Tracks guest cursor shape and visibility and applies them to the viewer. |
| `viewer/dmabuf.rs` | Imports, transforms, and presents DMABUF scanouts for accelerated rendering. |
| `viewer/framebuffer.rs` | Normalizes software framebuffer updates and emits presentation events. |
| `viewer/grab.rs` | Manages keyboard and mouse capture, release, and cursor grabbing behavior. |
| `viewer/hotkeys.rs` | Parses configurable hotkey definitions and matches them at runtime. |
| `viewer/keyboard.rs` | Translates GTK key events into QEMU qnum keycodes and forwards them to the guest. |
| `viewer/listener.rs` | Runs the async D-Bus listener thread, input forwarding, and reconnect supervision. |
| `viewer/mouse.rs` | Maps widget coordinates and pointer events into guest mouse actions. |
| `viewer/utils.rs` | Holds shared viewer helpers for sizing, icons, and small GTK utilities. |

## 📚 References

- QEMU D-Bus display documentation: <https://www.qemu.org/docs/master/interop/dbus-display.html>
- `qemu-display` Rust crate: <https://gitlab.com/marcandre.lureau/qemu-display>

## 🪪 License

*QD2* is released under the [GPL-3.0 LICENSE](./LICENSE)
