# QD2

QD2 stands for QEMU D-Bus Display. It is a Rust CLI for discovering and connecting to QEMU virtual machines that expose the `-display dbus` interface.

## Current status

The current implementation covers discovery, inspection, and a graphical viewer:

- `qd2 list` enumerates visible QEMU D-Bus VMs on the session bus and common libvirt private D-Bus socket directories.
- `qd2 inspect` shows VM metadata, console details, and exported helper interfaces.
- `qd2 connect` opens a GTK4 window for one console and renders both framebuffer and DMABUF scanouts when available.
- `qd2 connect` forwards keyboard and mouse input, tracks guest cursor shape updates, and syncs clipboard content.
- `qd2 connect --hotkeys ...` overrides viewer shortcuts such as fullscreen toggling and input release.
- `--address <DBUS_ADDRESS>` connects to a custom D-Bus bus instead of the session bus.

## Example

```bash
cargo run -- list
cargo run -- inspect
cargo run -- inspect --vm :1.421
cargo run -- inspect --vm demo-vm
cargo run -- list --address "unix:path=/tmp/qemu-bus"
cargo run -- inspect --address "unix:path=/run/libvirt/qemu/dbus/12-oscp-dbus.sock"
cargo run -- connect --address "unix:path=/run/libvirt/qemu/dbus/12-oscp-dbus.sock"
cargo run -- connect --address "unix:path=/run/libvirt/qemu/dbus/12-oscp-dbus.sock" \
  --hotkeys "toggle-fullscreen=ctrl+enter,release-cursor=ctrl+alt"
```

## References

- QEMU D-Bus display documentation: <https://www.qemu.org/docs/master/interop/dbus-display.html>
- `qemu-display` Rust crate: <https://gitlab.com/marcandre.lureau/qemu-display>
