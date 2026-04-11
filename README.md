# QD2

QD2 stands for QEMU D-Bus Display. It is a Rust CLI for discovering and connecting to QEMU virtual machines that expose the `-display dbus` interface.

## Current status

This first step focuses on discovery and inspection:

- `qd2 list` enumerates visible QEMU D-Bus VMs.
- `qd2 inspect` shows VM metadata, console details, and exported helper interfaces.
- `--address <DBUS_ADDRESS>` connects to a custom D-Bus bus instead of the session bus.

## Example

```bash
cargo run -- list
cargo run -- inspect
cargo run -- inspect --vm :1.421
cargo run -- inspect --vm demo-vm
cargo run -- list --address "unix:path=/tmp/qemu-bus"
```

## References

- QEMU D-Bus display documentation: <https://www.qemu.org/docs/master/interop/dbus-display.html>
- `qemu-display` Rust crate: <https://gitlab.com/marcandre.lureau/qemu-display>
