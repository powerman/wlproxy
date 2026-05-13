# Wlproxy

[![License MIT](https://img.shields.io/badge/license-MIT-royalblue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rustc-1.95.0-blue?logo=rust)](https://www.rust-lang.org)
[![Test](https://img.shields.io/github/actions/workflow/status/powerman/wlproxy/test.yml?label=test)](https://github.com/powerman/wlproxy/actions/workflows/test.yml)
[![Coverage Status](https://raw.githubusercontent.com/powerman/wlproxy/gh-badges/coverage.svg)](https://github.com/powerman/wlproxy/actions/workflows/test.yml)
[![Crates.io](https://img.shields.io/crates/v/wlproxy?logo=rust)](https://crates.io/crates/wlproxy)
[![Release](https://img.shields.io/github/v/release/powerman/wlproxy?color=blue)](https://github.com/powerman/wlproxy/releases/latest)

![Linux | amd64 arm64](https://img.shields.io/badge/Linux-amd64%20arm64-royalblue)
![macOS | amd64 arm64](https://img.shields.io/badge/macOS-amd64%20arm64-royalblue)

Wayland socket proxy that can do minor changes to messages for any programs
that use its downstream socket.

This allows you to do things like create a proxy Wayland socket to mount in a container
and write compositor decoration rules that are specific to the container windows.

## Features

- Replace or prefix `app_id` -
  this can help writing compositor rules targeting programs running on a wlproxy instance.
- Replace or prefix `title` - this may be helpful if nesting compositors,
  since compositors don't expect their title to be used and don't set useful titles.
- Block specific Wayland interfaces by name -
  prevents the client from binding to any of the listed interfaces.
  Blocked global events are silently dropped before reaching the client,
  and the client's bind requests for these interfaces are intercepted.
  This can be used to restrict access to capabilities like
  screenshots (`zwlr_screencopy_manager_v1`),
  clipboard (`ext_data_control_manager_v1`, `zwlr_data_control_device_v1`),
  layer shell (`zwlr_layer_shell_v1`), and others.

## Installation

### From source

```sh
cargo install wlproxy
```

### Pre-built binary

```sh
cargo binstall wlproxy
```

Or download a pre-built binary from the
[releases page](https://github.com/powerman/wlproxy/releases).

## Usage

```text
Usage: wlproxy [OPTIONS] <DOWNSTREAM>

Arguments:
  <DOWNSTREAM>  Full path for the new Wayland socket

Options:
  -u, --upstream <UPSTREAM>  Full path to compositor Wayland socket
  -a, --app-id <APP_ID>      Force all xdg toplevels to have the same app id
  -A, --prefix-app-id        Prefix the app id instead of replacing
  -t, --title <TITLE>        Force all xdg toplevels to have the same title
  -T, --prefix-title         Prefix the title instead of replacing
  -b, --block <BLOCK>        Wayland interfaces to block (can be specified multiple times)
  -q, --quiet                Suppress warnings about unknown interface names
      --debug                Print debug messages
  -h, --help                 Print help
```

### Basic passthrough

```sh
wlproxy /run/user/1000/wayland-filtered
WAYLAND_DISPLAY=wayland-filtered my-app
```

The `--upstream` flag is optional and defaults to
`$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY` (or `$XDG_RUNTIME_DIR/wayland-0`).

### Replace app_id

```sh
wlproxy /run/user/1000/wayland-filtered --app-id org.example.testid
```

### Prefix app_id

```sh
wlproxy /run/user/1000/wayland-filtered --app-id pfx- --prefix-app-id
```

### Block privacy-sensitive interfaces

When running untrusted applications (e.g. in a container or Flatpak),
you can block Wayland interfaces that could leak sensitive data
or compromise the user's session:

```sh
wlproxy /run/user/1000/wayland-filtered \
    --block zwlr_screencopy_manager_v1 \
    --block zkde_screencast_unstable_v1 \
    --block ext_data_control_manager_v1 \
    --block zwlr_data_control_manager_v1 \
    --block zwlr_virtual_pointer_manager_v1 \
    --block zwp_virtual_keyboard_manager_v1
```

This blocks the following capabilities:

| Interface                                                             | Risk             |
| --------------------------------------------------------------------- | ---------------- |
| `zwlr_screencopy_manager_v1` / `zkde_screencast_unstable_v1`          | Screen capture   |
| `ext_data_control_manager_v1` / `zwlr_data_control_manager_v1`        | Clipboard access |
| `zwlr_virtual_pointer_manager_v1` / `zwp_virtual_keyboard_manager_v1` | Input injection  |

The `--quiet` flag suppresses warnings about unknown interface names
(useful when listing interfaces that require specific compositor support).

## Acknowledgements

This project is a fork of [andrewbaxter/filterway](https://github.com/andrewbaxter/filterway/),
licensed under ISC.

I'm grateful to Andrew Baxter for the original implementation and inspiration.
The original project appears to be inactive,
and since I needed to introduce substantial changes,
a separate project fork proved to be a better fit than a pull request workflow.
The original author's code remains under the [ISC license](andrewbaxter-license.txt).
