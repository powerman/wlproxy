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

Current filters:

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

# How to use it

Your main compositor will have created something like `/run/user/1000/wayland-0` where `1000` is
your user ID.

1. Build `wlproxy` with `cargo build`.

   If you use `rustup` to manage rust it should read the `rust-toolchain.toml` file and compile
   accordingly.

2. Run `wlproxy --upstream /run/user/1000/wayland-0 --downstream /run/user/1000/wayland-filtered --app-id org.example.testid`

   Run `wlproxy --help` for details.

3. Run Wayland applications or another compositor with `WAYLAND_DISPLAY=wayland-filtered`

# Acknowledgements

This project is a fork of [andrewbaxter/filterway](https://github.com/andrewbaxter/filterway/), licensed under ISC.

I'm grateful to Andrew Baxter for the original implementation and inspiration.
The original project appears to be inactive,
and since I needed to introduce substantial changes,
a separate project fork proved to be a better fit than a pull request workflow.
The original author's code remains under the [ISC license](andrewbaxter-license.txt).
