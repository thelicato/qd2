# Contributing to QD2

Thanks for taking the time to contribute to QD2.

QD2 is a Rust + GTK4 client for QEMU's D-Bus display stack. The project is most useful when changes stay practical, easy to test, and friendly to real-world VM workflows.

## Before You Start

- Check existing issues before opening a new one.
- Open an issue or start a discussion before large changes, especially for new protocols, new host integrations, or UI changes with behavior tradeoffs.
- Keep contributions focused. Small, reviewable pull requests are strongly preferred over large rewrites.

## Development Setup

QD2 currently builds from source with:

- Rust stable with Cargo
- GTK4 development files
- pixman development files
- usbredir 0.13+ libraries visible to `pkg-config` (`libusbredirhost.pc` and `libusbredirparser-0.5.pc`)
- `pkg-config` or `pkgconf`

Typical package names:

- Debian/Ubuntu: `libgtk-4-dev`, `libpixman-1-dev`, `libusb-1.0-0-dev`, `pkg-config`, `meson`, `ninja-build`

If your distro packages do not provide the `usbredir` pkg-config files expected by `qemu-display`, install `usbredir` 0.13+ from source first.

Useful commands:

```bash
cargo fmt
cargo test --offline
cargo run -- --help
```

If you are working on viewer behavior, it is also helpful to test at least one real VM flow such as:

```bash
cargo run -- list
cargo run -- inspect --vm <vm>
cargo run -- doctor --vm <vm>
cargo run -- connect --vm <vm>
```

## Contribution Guidelines

- Prefer small, targeted changes.
- Preserve the existing GTK4 and viewer behavior unless the change intentionally improves it.
- Add or update tests when logic changes.
- Update the README or other docs when user-facing behavior changes.
- Keep module boundaries tidy. If a file starts getting too large, prefer splitting it into focused modules.
- Add comments sparingly and only where they help explain non-obvious behavior.

## Reporting Bugs Well

The best bug reports include:

- your host OS and desktop session
- how QD2 was launched
- the exact `qd2` command used
- whether you used `--address`
- relevant `qd2 doctor` output
- relevant `qd2 inspect` output
- QEMU command-line snippets when the issue depends on guest wiring
- logs or stderr output if the failure mentions audio, clipboard, permissions, or reconnects

If the problem depends on `sudo`, Wayland, PipeWire, private libvirt sockets, or clipboard/audio guest agents, please mention that explicitly.

## Pull Request Checklist

Before opening a pull request, please make sure:

- `cargo fmt` passes
- `cargo test --offline` passes
- the change is documented if it affects users
- the pull request description explains the user-visible behavior change
- screenshots or short recordings are included for meaningful UI changes when practical

## Scope

QD2 focuses on QEMU D-Bus display workflows. Contributions that improve reliability, diagnostics, viewer polish, platform support, or documentation are especially valuable.
