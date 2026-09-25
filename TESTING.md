# Testing i3mux

i3mux supports Sway only. The shell multiplexer is shpool 0.11.5.

## Entry points

| Command | What it runs | Needs |
|---|---|---|
| `just check` | fmt, clippy `-D warnings`, boundary rule, unit tests | Rust |
| `just check-mux` | pty tests against a real shpool | `shpool` on `PATH` or `SHPOOL_BIN` |
| `just check-int` | container integration suite | podman or docker, musl target |
| `just update-goldens` | integration suite, rewrites layout goldens | as above |
| `just images` | builds the test image | podman-compose or docker compose |
| `just clean-containers` | removes leftover test containers | podman or docker |

CI runs `just check`, `just check-mux` and `just check-int`. Nothing else.
Pass extra `cargo test` arguments after the recipe:
`just check-int layout -- --nocapture`.

## Tiers

1. **Unit.** Pure logic: types, state, window and layout over JSON
   fixtures of real `swaymsg -t get_tree` output, restore plans, shpool
   command building, shell quoting. No Sway, no network.
2. **pty.** Runs the shpool attach command that i3mux generates against a
   scratch socket. It prints 200 lines, detaches, and reattaches. It
   asserts that the first line comes back and that no alternate-screen
   sequence appears. A second test pins that shpool refuses a second
   client (exit status 0, message on stderr) and that the wrapper then
   shows the `--force` hint. These tests are `#[ignore]`d in `cargo test`
   and run under `just check-mux`; a missing shpool fails them, it never
   skips them.
3. **Integration.** Two containers from one image. One runs headless Sway
   and i3mux. The other runs sshd as the remote host. Tests cover
   activate, terminal, orphan reattach, detach and attach (local and
   remote), `--force`, kill, scrollback, and layout goldens.

## Boundary rule

`src/layout.rs` and `src/window.rs` must not import `crate::wm`. They take
sway JSON from the caller. This keeps them testable from fixtures.
`just check` fails with the file, line, and rule if you break this.

## Containers

- Image: `i3mux-test-sway:latest`, built from `tests/docker/Dockerfile`.
- Services: `i3mux-test-sway` (Sway client) and `i3mux-remote-ssh` (sshd).
- The image installs shpool to `/usr/local/bin`. The version and tarball
  checksum are `ARG`s at the top of the Dockerfile. Change both together.
- The tests copy the musl binary
  `target/x86_64-unknown-linux-musl/debug/i3mux` into the Sway container.
  `tests/integration.sh` builds it first.
- podman is the local default. Set `COMPOSE="docker compose"` to use
  docker.

## Golden images

Goldens live in `tests/integration/golden/`. To regenerate them, run
`just update-goldens`. Review every changed image before you commit.
CI never sets `UPDATE_GOLDENS`. On failure, the suite writes golden,
actual, and diff images to `tests/test-output/failures/`.
