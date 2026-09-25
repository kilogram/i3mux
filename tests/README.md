# i3mux container tests

The integration suite runs i3mux in headless Sway and checks what a user
sees: windows, marks, shells on each host, and screenshots. `TESTING.md`
at the repository root describes all test tiers.

## Run

```bash
just check-int                            # whole suite (builds the musl binary)
just check-int scrollback -- --nocapture  # one test, with output
just update-goldens                       # rewrite the golden images
just clean-containers                     # remove leftover containers
```

The suite needs podman or docker and the `x86_64-unknown-linux-musl`
target. With `DOCKER_HOST` pointing at a podman socket it uses podman and
podman-compose, otherwise docker and `docker compose`. It builds the image
`i3mux-test-sway:latest` when the image is missing.

## How a test runs

1. `TestEnvironment::new()` starts two fresh containers from one image. One
   runs headless Sway, the other runs sshd as the remote host
   (`i3mux-remote-ssh`, user `testuser`). Tests never share state.
2. The test runs `i3mux` inside the Sway container with foot as the
   terminal. Session-level tests run once for `Session::Local` and once
   for `Session::Remote`.
3. It checks Sway (`swaymsg -t get_tree`), the shells on each host
   (`shpool list --json` on the i3mux socket
   `~/.local/run/i3mux/shpool.socket`), host files, and screenshots.
4. Dropping the environment removes both containers.

## Layout

```
tests/
  common/            shared infrastructure
    docker.rs        containers, file setup, exec
    environment.rs   Sway queries, host shells, second clients, screenshots
    i3mux.rs         the i3mux CLI and the Local/Remote Session parameter
    screenshot.rs    golden comparison; diff_image.rs, comparison_spec.rs
  integration/       the test binary (main.rs lists the modules)
    golden/sway/     golden images
    golden/specs/    comparison thresholds and restore-* layout recipes
  color-scripts/     color-fill.sh and print-lines.sh, copied to both hosts
  docker/            Dockerfile, compose file, Sway config, msg and
                     launch_terminal helpers
```

## Golden images

Local and remote cases share one golden: the shells must look the same.
Each test's prompt is `$ ` on both hosts so that the random container
hostname never reaches an image. After `just update-goldens`, open every
changed image and check it shows the layout the test names. On failure the
suite writes golden, actual, diff and a report to
`tests/test-output/failures/`.

## Known bugs pinned by tests

These tests expect the current wrong behaviour and fail once it is fixed.
Then turn them into plain tests.

- `detach_attach::local_attach_right_after_detach_is_not_refused`
- `mux::pty_tests::refused_attach_exits_nonzero_so_the_wrapper_keeps_the_window_open`
  (in `src/`, run by `just check-mux`)
