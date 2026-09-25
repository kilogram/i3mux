//! The two test containers: headless Sway (the i3mux client) and sshd (the
//! remote host). Both run the image `i3mux-test-sway:latest`.
//!
//! The runtime is podman when `DOCKER_HOST` mentions podman, else docker.
//! testcontainers removes both containers when [`Containers`] is dropped.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use testcontainers::core::WaitFor;
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};

/// The test image, built from `tests/docker/Dockerfile`.
const IMAGE: &str = "i3mux-test-sway";
const TAG: &str = "latest";
/// The compose service that builds [`IMAGE`].
const SERVICE: &str = "i3mux-test-sway";

/// The ssh name of the remote container, as `--remote` takes it. The ssh
/// config written into the Sway container maps it to `testuser`.
pub const REMOTE_HOST: &str = "i3mux-remote-ssh";

/// Home directories of the i3mux user on each side.
pub const LOCAL_HOME: &str = "/root";
pub const REMOTE_HOME: &str = "/home/testuser";

/// Test scripts copied into both containers, from `tests/color-scripts`.
const SCRIPTS: [&str; 2] = ["color-fill.sh", "print-lines.sh"];
const SCRIPT_DIR: &str = "/opt/i3mux-test/color-scripts";

const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Container CLI and compose command.
struct Runtime {
    cli: &'static str,
    compose: &'static [&'static str],
}

fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        let podman = std::env::var("DOCKER_HOST").is_ok_and(|h| h.contains("podman"));
        if podman {
            Runtime {
                cli: "podman",
                compose: &["podman-compose"],
            }
        } else {
            Runtime {
                cli: "docker",
                compose: &["docker", "compose"],
            }
        }
    })
}

fn manifest_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

/// Run `command`; fail with its stderr if it exits non-zero.
fn run_checked(command: &mut Command, what: &str) -> Result<Output> {
    let output = command
        .output()
        .with_context(|| format!("cannot run: {what}"))?;
    if !output.status.success() {
        bail!(
            "{what} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

fn copy_to(container: &str, source: &Path, target: &str) -> Result<()> {
    run_checked(
        Command::new(runtime().cli)
            .arg("cp")
            .arg(source)
            .arg(format!("{container}:{target}")),
        &format!("copy {} to {target}", source.display()),
    )?;
    Ok(())
}

/// Build the image once per test process if the runtime does not have it.
fn ensure_image_built() -> Result<()> {
    static BUILT: OnceLock<()> = OnceLock::new();
    if BUILT.get().is_some() {
        return Ok(());
    }
    let rt = runtime();
    let listed = run_checked(
        Command::new(rt.cli).args(["images", "-q", &format!("{IMAGE}:{TAG}")]),
        "list images",
    )?;
    if listed.stdout.is_empty() {
        println!("Building {IMAGE}:{TAG} (one-time setup)...");
        let (program, args) = rt.compose.split_first().context("empty compose command")?;
        run_checked(
            Command::new(program)
                .args(args)
                .args(["build", SERVICE])
                .current_dir(manifest_path("tests/docker")),
            "build the test image",
        )?;
    }
    let _ = BUILT.set(());
    Ok(())
}

/// A running Sway container and sshd container, wired together.
pub struct Containers {
    sway: Container<GenericImage>,
    remote: Container<GenericImage>,
}

impl Containers {
    /// Start both containers and wait until Sway and ssh answer.
    pub fn start() -> Result<Self> {
        ensure_image_built()?;
        let remote = GenericImage::new(IMAGE, TAG)
            .with_wait_for(WaitFor::message_on_stderr("Server listening"))
            .with_cmd(["/usr/sbin/sshd", "-D", "-e"])
            .start()
            .context("start the sshd container")?;
        let sway = GenericImage::new(IMAGE, TAG)
            .with_wait_for(WaitFor::message_on_stdout("Test environment is ready!"))
            .with_cmd(["/opt/i3mux-test/start-sway.sh"])
            .start()
            .context("start the Sway container")?;
        let containers = Self { sway, remote };
        containers.install_files()?;
        containers.connect_ssh()?;
        containers.wait_until_ready()?;
        Ok(containers)
    }

    fn install_files(&self) -> Result<()> {
        let binary = manifest_path("target/x86_64-unknown-linux-musl/debug/i3mux");
        if !binary.exists() {
            bail!(
                "{} is missing; run tests/integration.sh (it builds the musl binary first)",
                binary.display()
            );
        }
        copy_to(self.sway.id(), &binary, "/usr/local/bin/i3mux")?;
        for id in [self.sway.id(), self.remote.id()] {
            for script in SCRIPTS {
                let source = manifest_path(&format!("tests/color-scripts/{script}"));
                copy_to(id, &source, &format!("{SCRIPT_DIR}/{script}"))?;
            }
        }
        self.sway_ok(&format!("chmod 755 /usr/local/bin/i3mux {SCRIPT_DIR}/*"))?;
        self.remote_root_ok(&format!("chmod 755 {SCRIPT_DIR}/*"))?;
        // Same short prompt for both users: goldens do not depend on the
        // random container hostname, and local and remote look alike.
        self.sway_ok(&format!("echo \"PS1='$ '\" >> {LOCAL_HOME}/.bashrc"))?;
        self.remote_root_ok(&format!(
            "echo \"PS1='$ '\" >> {REMOTE_HOME}/.bashrc && chown testuser: {REMOTE_HOME}/.bashrc"
        ))?;
        Ok(())
    }

    /// Key, ssh config and `/etc/hosts` entry so that `ssh i3mux-remote-ssh`
    /// from the Sway container reaches the remote as testuser.
    fn connect_ssh(&self) -> Result<()> {
        let key = manifest_path("tests/docker/ssh-keys/id_rsa");
        let public = manifest_path("tests/docker/ssh-keys/id_rsa.pub");
        copy_to(self.sway.id(), &key, "/root/.ssh/id_rsa")?;
        self.sway_ok(&format!(
            "chmod 700 /root/.ssh && chmod 600 /root/.ssh/id_rsa && cat > /root/.ssh/config <<'EOF'\n\
             Host {REMOTE_HOST}\n  HostName {REMOTE_HOST}\n  User testuser\n  IdentityFile /root/.ssh/id_rsa\n\
             \x20 BatchMode yes\n  StrictHostKeyChecking no\n  UserKnownHostsFile /dev/null\n  LogLevel ERROR\nEOF\n\
             chmod 600 /root/.ssh/config"
        ))?;
        copy_to(
            self.remote.id(),
            &public,
            &format!("{REMOTE_HOME}/.ssh/authorized_keys"),
        )?;
        self.remote_root_ok(&format!(
            "chown -R testuser: {REMOTE_HOME}/.ssh && chmod 700 {REMOTE_HOME}/.ssh \
             && chmod 600 {REMOTE_HOME}/.ssh/authorized_keys"
        ))?;

        let inspect = run_checked(
            Command::new(runtime().cli).args([
                "inspect",
                "-f",
                "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
                self.remote.id(),
            ]),
            "inspect the sshd container",
        )?;
        let ip = String::from_utf8_lossy(&inspect.stdout).trim().to_owned();
        if ip.is_empty() {
            bail!("the sshd container has no IP address");
        }
        self.sway_ok(&format!("echo '{ip} {REMOTE_HOST}' >> /etc/hosts"))?;
        Ok(())
    }

    fn wait_until_ready(&self) -> Result<()> {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            let sway = self.exec_in_sway("source /tmp/sway-env.sh && swaymsg -t get_version")?;
            let ssh = self.exec_in_sway(&format!("ssh {REMOTE_HOST} true"))?;
            if sway.status.success() && ssh.status.success() {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!(
                    "containers not ready after {READY_TIMEOUT:?}: sway: {} ssh: {}",
                    String::from_utf8_lossy(&sway.stderr).trim(),
                    String::from_utf8_lossy(&ssh.stderr).trim()
                );
            }
            thread::sleep(Duration::from_millis(500));
        }
    }

    /// Copy `path` out of the Sway container to `target` on this machine.
    pub fn copy_from_sway(&self, path: &str, target: &Path) -> Result<()> {
        run_checked(
            Command::new(runtime().cli)
                .arg("cp")
                .arg(format!("{}:{path}", self.sway.id()))
                .arg(target),
            &format!("copy {path} out of the Sway container"),
        )?;
        Ok(())
    }

    /// `bash -c CMD` as root in the Sway container.
    pub fn exec_in_sway(&self, cmd: &str) -> Result<Output> {
        Command::new(runtime().cli)
            .args(["exec", self.sway.id(), "bash", "-c", cmd])
            .output()
            .context("exec in the Sway container")
    }

    /// `bash -lc CMD` as testuser in the sshd container, with the
    /// environment an ssh session has (no `XDG_RUNTIME_DIR`).
    pub fn exec_in_remote(&self, cmd: &str) -> Result<Output> {
        Command::new(runtime().cli)
            .args(["exec", "-u", "testuser", "-e", "HOME=/home/testuser"])
            .args([
                self.remote.id(),
                "env",
                "-u",
                "XDG_RUNTIME_DIR",
                "bash",
                "-lc",
                cmd,
            ])
            .output()
            .context("exec in the sshd container")
    }

    /// Start `bash -c CMD` in the background: in the Sway container as root
    /// (`remote == false`) or in the sshd container as testuser.
    pub fn spawn_detached(&self, remote: bool, cmd: &str) -> Result<()> {
        let mut command = Command::new(runtime().cli);
        command.args(["exec", "-d"]);
        if remote {
            command
                .args(["-u", "testuser", "-e", "HOME=/home/testuser"])
                .args([self.remote.id(), "env", "-u", "XDG_RUNTIME_DIR"]);
        } else {
            command.arg(self.sway.id());
        }
        run_checked(
            command.args(["bash", "-c", cmd]),
            "start a background command",
        )?;
        Ok(())
    }

    fn sway_ok(&self, cmd: &str) -> Result<()> {
        run_checked(
            Command::new(runtime().cli).args(["exec", self.sway.id(), "bash", "-c", cmd]),
            cmd,
        )?;
        Ok(())
    }

    fn remote_root_ok(&self, cmd: &str) -> Result<()> {
        run_checked(
            Command::new(runtime().cli).args(["exec", self.remote.id(), "bash", "-c", cmd]),
            cmd,
        )?;
        Ok(())
    }
}
