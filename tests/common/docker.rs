// Container management using testcontainers-rs (v0.23 API)

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;
use testcontainers::{core::WaitFor, runners::SyncRunner, GenericImage, ImageExt};

/// Container runtime configuration - detected once, used everywhere
struct ContainerRuntime {
    /// CLI command: "docker" or "podman"
    cli: &'static str,
    /// Compose command: "docker-compose" or "podman-compose"
    compose: &'static str,
}

static RUNTIME: OnceLock<ContainerRuntime> = OnceLock::new();

fn runtime() -> &'static ContainerRuntime {
    RUNTIME.get_or_init(|| {
        // Check DOCKER_HOST to see if podman socket is configured
        let use_podman = std::env::var("DOCKER_HOST")
            .map(|h| h.contains("podman"))
            .unwrap_or(false);

        if use_podman {
            ContainerRuntime {
                cli: "podman",
                compose: "podman-compose",
            }
        } else {
            ContainerRuntime {
                cli: "docker",
                compose: "docker-compose",
            }
        }
    })
}

pub struct ContainerManager {
    xvfb_container: testcontainers::Container<GenericImage>,
    remote_container: testcontainers::Container<GenericImage>,
}

impl ContainerManager {
    pub fn new() -> Result<Self> {
        // Build images ONCE (they'll be cached by docker/podman)
        Self::ensure_images_built()?;

        let image_name = Self::get_image_name();

        // Create Xvfb container - testcontainers handles lifecycle
        let xvfb_container = GenericImage::new(image_name.clone(), "latest".to_string())
            .with_wait_for(WaitFor::message_on_stdout("Test environment is ready!"))
            .with_cmd(["/opt/i3mux-test/start-xephyr.sh"])
            .start()?;

        // Create SSH remote container (same image, different command)
        let remote_container = GenericImage::new(image_name, "latest".to_string())
            .with_wait_for(WaitFor::message_on_stderr("Server listening"))
            .with_cmd(["/usr/sbin/sshd", "-D", "-e"])
            .start()?;

        let mgr = Self {
            xvfb_container,
            remote_container,
        };

        // Copy i3mux binary and test scripts into containers
        mgr.setup_container_files()?;

        // Setup networking - add remote container to xephyr's hosts file
        mgr.setup_networking()?;

        Ok(mgr)
    }

    fn setup_container_files(&self) -> Result<()> {
        let cli = runtime().cli;
        let manifest_dir = env!("CARGO_MANIFEST_DIR");

        // Copy i3mux binary to xvfb container (use statically-linked musl binary)
        let i3mux_binary = PathBuf::from(manifest_dir).join("target/x86_64-unknown-linux-musl/debug/i3mux");
        if !i3mux_binary.exists() {
            anyhow::bail!("i3mux musl binary not found.\nRun: cargo build --target x86_64-unknown-linux-musl");
        }

        let xvfb_id = self.xvfb_container.id();
        Command::new(cli)
            .args(&[
                "cp",
                i3mux_binary.to_str().unwrap(),
                &format!("{}:/usr/local/bin/i3mux", xvfb_id),
            ])
            .status()
            .context("Failed to copy i3mux binary to xvfb container")?;

        // Make it executable
        self.exec_in_xephyr("chmod +x /usr/local/bin/i3mux")?;

        // Copy color-fill.sh script to xvfb container
        self.exec_in_xephyr("mkdir -p /opt/i3mux-test/color-scripts")?;

        let color_fill_script = PathBuf::from(manifest_dir).join("tests/color-scripts/color-fill.sh");
        Command::new(cli)
            .args(&[
                "cp",
                color_fill_script.to_str().unwrap(),
                &format!("{}:/opt/i3mux-test/color-scripts/color-fill.sh", xvfb_id),
            ])
            .status()
            .context("Failed to copy color-fill.sh to xvfb container")?;

        self.exec_in_xephyr("chmod +x /opt/i3mux-test/color-scripts/color-fill.sh")?;

        // Copy SSH keys for remote connections
        self.exec_in_xephyr("mkdir -p /root/.ssh/sockets")?;

        let ssh_key = PathBuf::from(manifest_dir).join("tests/docker/ssh-keys/id_rsa");
        let ssh_pub = PathBuf::from(manifest_dir).join("tests/docker/ssh-keys/id_rsa.pub");

        Command::new(cli)
            .args(&[
                "cp",
                ssh_key.to_str().unwrap(),
                &format!("{}:/root/.ssh/id_rsa", xvfb_id),
            ])
            .status()
            .context("Failed to copy SSH private key to xvfb container")?;

        Command::new(cli)
            .args(&[
                "cp",
                ssh_pub.to_str().unwrap(),
                &format!("{}:/root/.ssh/id_rsa.pub", xvfb_id),
            ])
            .status()
            .context("Failed to copy SSH public key to xvfb container")?;

        // Set proper permissions for SSH keys
        self.exec_in_xephyr("chmod 600 /root/.ssh/id_rsa")?;
        self.exec_in_xephyr("chmod 644 /root/.ssh/id_rsa.pub")?;
        self.exec_in_xephyr("chmod 700 /root/.ssh")?;

        // Create SSH config (overwrite the one from Dockerfile to ensure proper settings)
        let ssh_config = r#"
Host i3mux-remote-ssh
  HostName i3mux-remote-ssh
  User testuser
  Port 22
  IdentityFile /root/.ssh/id_rsa
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
  ControlMaster auto
  ControlPath /root/.ssh/sockets/%r@%h:%p
  ControlPersist 600
"#;

        let config_cmd = format!(
            "cat > /root/.ssh/config << 'EOF'\n{}EOF\nchmod 600 /root/.ssh/config",
            ssh_config
        );
        self.exec_in_xephyr(&config_cmd)?;

        // Copy public key to remote container for SSH authentication
        let remote_id = self.remote_container.id();

        // Create .ssh directory for testuser
        Command::new(cli)
            .args(&["exec", remote_id, "bash", "-c", "mkdir -p /home/testuser/.ssh && chown testuser:testuser /home/testuser/.ssh && chmod 700 /home/testuser/.ssh"])
            .status()
            .context("Failed to create .ssh directory in remote container")?;

        // Copy public key to remote container
        Command::new(cli)
            .args(&[
                "cp",
                ssh_pub.to_str().unwrap(),
                &format!("{}:/home/testuser/.ssh/authorized_keys", remote_id),
            ])
            .status()
            .context("Failed to copy public key to remote container")?;

        // Set proper permissions on authorized_keys
        Command::new(cli)
            .args(&["exec", remote_id, "bash", "-c", "chown testuser:testuser /home/testuser/.ssh/authorized_keys && chmod 600 /home/testuser/.ssh/authorized_keys"])
            .status()
            .context("Failed to set permissions on authorized_keys in remote container")?;

        Ok(())
    }

    fn ensure_images_built() -> Result<()> {
        let rt = runtime();
        let image_name = format!("{}:latest", Self::get_image_name());
        let check = Command::new(rt.cli)
            .args(&["images", "-q", &image_name])
            .output()?;

        if check.stdout.is_empty() {
            // Image doesn't exist, build it
            println!("Building container image (one-time setup)...");
            let docker_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/docker");

            let status = Command::new(rt.compose)
                .current_dir(&docker_dir)
                .args(&["build"])
                .status()
                .context("Failed to build image")?;

            if !status.success() {
                anyhow::bail!("Image build failed");
            }
            println!("✓ Image built and cached");
        } else {
            println!("✓ Using cached container image");
        }

        Ok(())
    }

    fn setup_networking(&self) -> Result<()> {
        let cli = runtime().cli;
        let remote_id = self.remote_container.id();

        // Get the IP address of the remote container
        let inspect_output = Command::new(cli)
            .args(&[
                "inspect",
                "-f",
                "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
                remote_id,
            ])
            .output()
            .context("Failed to inspect remote container")?;

        let remote_ip = String::from_utf8_lossy(&inspect_output.stdout)
            .trim()
            .to_string();

        if remote_ip.is_empty() {
            anyhow::bail!("Could not get IP address of remote container");
        }

        // Add the remote container's IP to xephyr's /etc/hosts
        let hosts_entry = format!("{} i3mux-remote-ssh", remote_ip);
        let add_hosts_cmd = format!("echo '{}' >> /etc/hosts", hosts_entry);

        self.exec_in_xephyr(&add_hosts_cmd)?;

        println!("✓ Configured network: {} -> {}", "i3mux-remote-ssh", remote_ip);

        Ok(())
    }

    fn get_image_name() -> String {
        // Use short name - both docker and podman can find local images this way
        // Note: podman stores as localhost/i3mux-test but finds it via short name
        "i3mux-test".to_string()
    }

    pub fn exec_in_xephyr(&self, cmd: &str) -> Result<std::process::Output> {
        let container_id = self.xvfb_container.id();
        Command::new(runtime().cli)
            .args(&["exec", container_id, "bash", "-c", cmd])
            .output()
            .context("Failed to exec in Xvfb container")
    }

    pub fn exec_in_remote(&self, cmd: &str) -> Result<std::process::Output> {
        let container_id = self.remote_container.id();
        Command::new(runtime().cli)
            .args(&["exec", container_id, "bash", "-c", cmd])
            .output()
            .context("Failed to exec in remote container")
    }

    pub fn wait_for_xephyr_ready(&self, timeout_secs: u64) -> Result<()> {
        println!("Waiting for Xvfb and i3 to be ready...");

        for attempt in 0..timeout_secs {
            let output = self.exec_in_xephyr("DISPLAY=:99 i3-msg -t get_workspaces 2>/dev/null")?;

            if output.status.success() {
                println!("✓ Xvfb and i3 are ready!");
                return Ok(());
            }

            if attempt % 5 == 0 && attempt > 0 {
                println!("  Still waiting... ({}/{}s)", attempt, timeout_secs);
            }

            thread::sleep(Duration::from_secs(1));
        }

        anyhow::bail!("Xvfb/i3 failed to start within {} seconds", timeout_secs)
    }

    pub fn wait_for_ssh_ready(&self, timeout_secs: u64) -> Result<()> {
        println!("Waiting for SSH server to be ready...");

        for attempt in 0..timeout_secs {
            let output = self.exec_in_remote("pgrep sshd >/dev/null 2>&1")?;

            if output.status.success() {
                println!("✓ SSH server is ready!");
                return Ok(());
            }

            if attempt % 5 == 0 && attempt > 0 {
                println!("  Still waiting... ({}/{}s)", attempt, timeout_secs);
            }

            thread::sleep(Duration::from_secs(1));
        }

        anyhow::bail!("SSH server failed to start within {} seconds", timeout_secs)
    }

    pub fn copy_from_xephyr(&self, container_path: &str, host_path: &str) -> Result<()> {
        let container_id = self.xvfb_container.id();
        let status = Command::new(runtime().cli)
            .args(&[
                "cp",
                &format!("{}:{}", container_id, container_path),
                host_path,
            ])
            .status()
            .context("Failed to copy file from container")?;

        if !status.success() {
            anyhow::bail!("Copy failed");
        }

        Ok(())
    }
}

// Testcontainers automatically cleans up containers when Container is dropped!
