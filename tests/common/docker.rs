// Container management using testcontainers-rs (v0.23 API)

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;
use testcontainers::{core::WaitFor, runners::SyncRunner, GenericImage, ImageExt};

/// Window manager type for testing
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestWmType {
    I3,
    Sway,
}

impl TestWmType {
    /// Detect WM type from I3MUX_TEST_WM environment variable
    pub fn from_env() -> Self {
        match std::env::var("I3MUX_TEST_WM").as_deref() {
            Ok("sway") => TestWmType::Sway,
            _ => TestWmType::I3,
        }
    }
}

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
    wm_container: testcontainers::Container<GenericImage>,
    remote_container: testcontainers::Container<GenericImage>,
    wm_type: TestWmType,
}

impl ContainerManager {
    pub fn new() -> Result<Self> {
        let wm_type = TestWmType::from_env();
        println!("Testing with WM type: {:?}", wm_type);

        // Build images ONCE (they'll be cached by docker/podman)
        Self::ensure_images_built(wm_type)?;

        let image_name = Self::get_image_name(wm_type);
        let start_script = Self::get_start_script(wm_type);

        // Create WM container (Xvfb/i3 or headless Sway)
        let wm_container = GenericImage::new(image_name.clone(), "latest".to_string())
            .with_wait_for(WaitFor::message_on_stdout("Test environment is ready!"))
            .with_cmd([start_script])
            .start()?;

        // Create SSH remote container (same image, different command)
        let remote_container = GenericImage::new(image_name, "latest".to_string())
            .with_wait_for(WaitFor::message_on_stderr("Server listening"))
            .with_cmd(["/usr/sbin/sshd", "-D", "-e"])
            .start()?;

        let mgr = Self {
            wm_container,
            remote_container,
            wm_type,
        };

        // Copy i3mux binary and test scripts into containers
        mgr.setup_container_files()?;

        // Setup networking - add remote container to WM container's hosts file
        mgr.setup_networking()?;

        Ok(mgr)
    }

    /// Get the WM type being tested
    pub fn wm_type(&self) -> TestWmType {
        self.wm_type
    }

    fn setup_container_files(&self) -> Result<()> {
        let cli = runtime().cli;
        let manifest_dir = env!("CARGO_MANIFEST_DIR");

        // Copy i3mux binary to WM container (use statically-linked musl binary)
        let i3mux_binary = PathBuf::from(manifest_dir).join("target/x86_64-unknown-linux-musl/debug/i3mux");
        if !i3mux_binary.exists() {
            anyhow::bail!("i3mux musl binary not found.\nRun: cargo build --target x86_64-unknown-linux-musl");
        }

        let wm_id = self.wm_container.id();
        Command::new(cli)
            .args(&[
                "cp",
                i3mux_binary.to_str().unwrap(),
                &format!("{}:/usr/local/bin/i3mux", wm_id),
            ])
            .status()
            .context("Failed to copy i3mux binary to WM container")?;

        // Make it executable
        self.exec_in_wm("chmod +x /usr/local/bin/i3mux")?;

        // Copy color-fill.sh script to WM container
        self.exec_in_wm("mkdir -p /opt/i3mux-test/color-scripts")?;

        let color_fill_script = PathBuf::from(manifest_dir).join("tests/color-scripts/color-fill.sh");
        Command::new(cli)
            .args(&[
                "cp",
                color_fill_script.to_str().unwrap(),
                &format!("{}:/opt/i3mux-test/color-scripts/color-fill.sh", wm_id),
            ])
            .status()
            .context("Failed to copy color-fill.sh to WM container")?;

        self.exec_in_wm("chmod +x /opt/i3mux-test/color-scripts/color-fill.sh")?;

        // Copy SSH keys for remote connections
        self.exec_in_wm("mkdir -p /root/.ssh/sockets")?;

        let ssh_key = PathBuf::from(manifest_dir).join("tests/docker/ssh-keys/id_rsa");
        let ssh_pub = PathBuf::from(manifest_dir).join("tests/docker/ssh-keys/id_rsa.pub");

        Command::new(cli)
            .args(&[
                "cp",
                ssh_key.to_str().unwrap(),
                &format!("{}:/root/.ssh/id_rsa", wm_id),
            ])
            .status()
            .context("Failed to copy SSH private key to WM container")?;

        Command::new(cli)
            .args(&[
                "cp",
                ssh_pub.to_str().unwrap(),
                &format!("{}:/root/.ssh/id_rsa.pub", wm_id),
            ])
            .status()
            .context("Failed to copy SSH public key to WM container")?;

        // Set proper permissions for SSH keys
        self.exec_in_wm("chmod 600 /root/.ssh/id_rsa")?;
        self.exec_in_wm("chmod 644 /root/.ssh/id_rsa.pub")?;
        self.exec_in_wm("chmod 700 /root/.ssh")?;

        // Create SSH config - hostname depends on WM type
        let ssh_hostname = match self.wm_type {
            TestWmType::I3 => "i3mux-remote-ssh",
            TestWmType::Sway => "i3mux-remote-ssh",  // Same for now, networking handles it
        };
        let ssh_config = format!(r#"
Host i3mux-remote-ssh
  HostName {}
  User testuser
  Port 22
  IdentityFile /root/.ssh/id_rsa
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
  ControlMaster auto
  ControlPath /root/.ssh/sockets/%r@%h:%p
  ControlPersist 600
"#, ssh_hostname);

        let config_cmd = format!(
            "cat > /root/.ssh/config << 'EOF'\n{}EOF\nchmod 600 /root/.ssh/config",
            ssh_config
        );
        self.exec_in_wm(&config_cmd)?;

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

    fn ensure_images_built(wm_type: TestWmType) -> Result<()> {
        let rt = runtime();
        let image_name = format!("{}:latest", Self::get_image_name(wm_type));
        let check = Command::new(rt.cli)
            .args(&["images", "-q", &image_name])
            .output()?;

        if check.stdout.is_empty() {
            // Image doesn't exist, build it
            println!("Building container image for {:?} (one-time setup)...", wm_type);
            let docker_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/docker");

            // Build the specific service based on WM type
            let services = match wm_type {
                TestWmType::I3 => vec!["i3mux-test-xephyr", "i3mux-remote-ssh"],
                TestWmType::Sway => vec!["i3mux-test-sway", "i3mux-remote-ssh-sway"],
            };

            for service in services {
                let status = Command::new(rt.compose)
                    .current_dir(&docker_dir)
                    .args(&["build", service])
                    .status()
                    .context(format!("Failed to build {} image", service))?;

                if !status.success() {
                    anyhow::bail!("Image build failed for {}", service);
                }
            }
            println!("✓ Images built and cached");
        } else {
            println!("✓ Using cached container image for {:?}", wm_type);
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

        // Add the remote container's IP to WM container's /etc/hosts
        let hosts_entry = format!("{} i3mux-remote-ssh", remote_ip);
        let add_hosts_cmd = format!("echo '{}' >> /etc/hosts", hosts_entry);

        self.exec_in_wm(&add_hosts_cmd)?;

        println!("✓ Configured network: {} -> {}", "i3mux-remote-ssh", remote_ip);

        Ok(())
    }

    fn get_image_name(wm_type: TestWmType) -> String {
        // Use short name - both docker and podman can find local images this way
        match wm_type {
            TestWmType::I3 => "i3mux-test".to_string(),
            TestWmType::Sway => "i3mux-test-sway".to_string(),
        }
    }

    fn get_start_script(wm_type: TestWmType) -> String {
        match wm_type {
            TestWmType::I3 => "/opt/i3mux-test/start-xephyr.sh".to_string(),
            TestWmType::Sway => "/opt/i3mux-test/start-sway.sh".to_string(),
        }
    }

    pub fn exec_in_wm(&self, cmd: &str) -> Result<std::process::Output> {
        let container_id = self.wm_container.id();
        Command::new(runtime().cli)
            .args(&["exec", container_id, "bash", "-c", cmd])
            .output()
            .context("Failed to exec in WM container")
    }

    pub fn exec_in_remote(&self, cmd: &str) -> Result<std::process::Output> {
        let container_id = self.remote_container.id();
        Command::new(runtime().cli)
            .args(&["exec", container_id, "bash", "-c", cmd])
            .output()
            .context("Failed to exec in remote container")
    }

    pub fn wait_for_wm_ready(&self, timeout_secs: u64) -> Result<()> {
        let (wm_name, check_cmd) = match self.wm_type {
            TestWmType::I3 => ("i3", "DISPLAY=:99 i3-msg -t get_workspaces 2>/dev/null"),
            TestWmType::Sway => ("Sway", "source /tmp/sway-env.sh && swaymsg -t get_workspaces 2>/dev/null"),
        };

        println!("Waiting for {} to be ready...", wm_name);

        for attempt in 0..timeout_secs {
            let output = self.exec_in_wm(check_cmd)?;

            if output.status.success() {
                println!("✓ {} is ready!", wm_name);
                return Ok(());
            }

            if attempt % 5 == 0 && attempt > 0 {
                println!("  Still waiting... ({}/{}s)", attempt, timeout_secs);
            }

            thread::sleep(Duration::from_secs(1));
        }

        anyhow::bail!("{} failed to start within {} seconds", wm_name, timeout_secs)
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

    pub fn copy_from_wm(&self, container_path: &str, host_path: &str) -> Result<()> {
        let container_id = self.wm_container.id();
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

/// Dual container manager for cross-WM testing
/// Runs both i3 and Sway containers with a shared remote SSH container
pub struct DualContainerManager {
    i3_container: testcontainers::Container<GenericImage>,
    sway_container: testcontainers::Container<GenericImage>,
    remote_container: testcontainers::Container<GenericImage>,
}

impl DualContainerManager {
    pub fn new() -> Result<Self> {
        println!("Creating dual WM test environment (i3 + Sway)...");

        // Build both image sets
        ContainerManager::ensure_images_built(TestWmType::I3)?;
        ContainerManager::ensure_images_built(TestWmType::Sway)?;

        // Create i3 container
        let i3_container = GenericImage::new("i3mux-test".to_string(), "latest".to_string())
            .with_wait_for(WaitFor::message_on_stdout("Test environment is ready!"))
            .with_cmd(["/opt/i3mux-test/start-xephyr.sh"])
            .start()?;

        // Create Sway container
        let sway_container = GenericImage::new("i3mux-test-sway".to_string(), "latest".to_string())
            .with_wait_for(WaitFor::message_on_stdout("Test environment is ready!"))
            .with_cmd(["/opt/i3mux-test/start-sway.sh"])
            .start()?;

        // Create shared SSH remote container (use i3 image - both have same SSH setup)
        let remote_container = GenericImage::new("i3mux-test".to_string(), "latest".to_string())
            .with_wait_for(WaitFor::message_on_stderr("Server listening"))
            .with_cmd(["/usr/sbin/sshd", "-D", "-e"])
            .start()?;

        let mgr = Self {
            i3_container,
            sway_container,
            remote_container,
        };

        // Setup files and networking for both WM containers
        mgr.setup_container_files(TestWmType::I3)?;
        mgr.setup_container_files(TestWmType::Sway)?;
        mgr.setup_networking()?;

        Ok(mgr)
    }

    fn wm_container(&self, wm_type: TestWmType) -> &testcontainers::Container<GenericImage> {
        match wm_type {
            TestWmType::I3 => &self.i3_container,
            TestWmType::Sway => &self.sway_container,
        }
    }

    fn setup_container_files(&self, wm_type: TestWmType) -> Result<()> {
        let cli = runtime().cli;
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let wm_id = self.wm_container(wm_type).id();

        // Copy i3mux binary
        let i3mux_binary = PathBuf::from(manifest_dir).join("target/x86_64-unknown-linux-musl/debug/i3mux");
        if !i3mux_binary.exists() {
            anyhow::bail!("i3mux musl binary not found.\nRun: cargo build --target x86_64-unknown-linux-musl");
        }

        Command::new(cli)
            .args(&["cp", i3mux_binary.to_str().unwrap(), &format!("{}:/usr/local/bin/i3mux", wm_id)])
            .status()
            .context("Failed to copy i3mux binary")?;

        self.exec_in_wm(wm_type, "chmod +x /usr/local/bin/i3mux")?;

        // Copy color-fill.sh script
        self.exec_in_wm(wm_type, "mkdir -p /opt/i3mux-test/color-scripts")?;
        let color_fill_script = PathBuf::from(manifest_dir).join("tests/color-scripts/color-fill.sh");
        Command::new(cli)
            .args(&["cp", color_fill_script.to_str().unwrap(), &format!("{}:/opt/i3mux-test/color-scripts/color-fill.sh", wm_id)])
            .status()
            .context("Failed to copy color-fill.sh")?;
        self.exec_in_wm(wm_type, "chmod +x /opt/i3mux-test/color-scripts/color-fill.sh")?;

        // Copy SSH keys
        self.exec_in_wm(wm_type, "mkdir -p /root/.ssh/sockets")?;
        let ssh_key = PathBuf::from(manifest_dir).join("tests/docker/ssh-keys/id_rsa");
        let ssh_pub = PathBuf::from(manifest_dir).join("tests/docker/ssh-keys/id_rsa.pub");

        Command::new(cli)
            .args(&["cp", ssh_key.to_str().unwrap(), &format!("{}:/root/.ssh/id_rsa", wm_id)])
            .status()?;
        Command::new(cli)
            .args(&["cp", ssh_pub.to_str().unwrap(), &format!("{}:/root/.ssh/id_rsa.pub", wm_id)])
            .status()?;

        self.exec_in_wm(wm_type, "chmod 600 /root/.ssh/id_rsa")?;
        self.exec_in_wm(wm_type, "chmod 644 /root/.ssh/id_rsa.pub")?;
        self.exec_in_wm(wm_type, "chmod 700 /root/.ssh")?;

        // Create SSH config
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
        self.exec_in_wm(wm_type, &config_cmd)?;

        Ok(())
    }

    fn setup_networking(&self) -> Result<()> {
        let cli = runtime().cli;
        let remote_id = self.remote_container.id();

        // Get remote container IP
        let inspect_output = Command::new(cli)
            .args(&["inspect", "-f", "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}", remote_id])
            .output()
            .context("Failed to inspect remote container")?;

        let remote_ip = String::from_utf8_lossy(&inspect_output.stdout).trim().to_string();
        if remote_ip.is_empty() {
            anyhow::bail!("Could not get IP address of remote container");
        }

        // Add to both WM containers
        let hosts_entry = format!("{} i3mux-remote-ssh", remote_ip);
        let add_hosts_cmd = format!("echo '{}' >> /etc/hosts", hosts_entry);

        self.exec_in_wm(TestWmType::I3, &add_hosts_cmd)?;
        self.exec_in_wm(TestWmType::Sway, &add_hosts_cmd)?;

        // Setup SSH authorized_keys on remote
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let ssh_pub = PathBuf::from(manifest_dir).join("tests/docker/ssh-keys/id_rsa.pub");

        Command::new(cli)
            .args(&["exec", remote_id, "bash", "-c", "mkdir -p /home/testuser/.ssh && chown testuser:testuser /home/testuser/.ssh && chmod 700 /home/testuser/.ssh"])
            .status()?;
        Command::new(cli)
            .args(&["cp", ssh_pub.to_str().unwrap(), &format!("{}:/home/testuser/.ssh/authorized_keys", remote_id)])
            .status()?;
        Command::new(cli)
            .args(&["exec", remote_id, "bash", "-c", "chown testuser:testuser /home/testuser/.ssh/authorized_keys && chmod 600 /home/testuser/.ssh/authorized_keys"])
            .status()?;

        println!("✓ Configured networking for both WMs -> {}", remote_ip);
        Ok(())
    }

    pub fn exec_in_wm(&self, wm_type: TestWmType, cmd: &str) -> Result<std::process::Output> {
        let container_id = self.wm_container(wm_type).id();
        Command::new(runtime().cli)
            .args(&["exec", container_id, "bash", "-c", cmd])
            .output()
            .context("Failed to exec in WM container")
    }

    pub fn exec_in_remote(&self, cmd: &str) -> Result<std::process::Output> {
        let container_id = self.remote_container.id();
        Command::new(runtime().cli)
            .args(&["exec", container_id, "bash", "-c", cmd])
            .output()
            .context("Failed to exec in remote container")
    }

    pub fn wait_for_wm_ready(&self, wm_type: TestWmType, timeout_secs: u64) -> Result<()> {
        let (wm_name, check_cmd) = match wm_type {
            TestWmType::I3 => ("i3", "DISPLAY=:99 i3-msg -t get_workspaces 2>/dev/null"),
            TestWmType::Sway => ("Sway", "source /tmp/sway-env.sh && swaymsg -t get_workspaces 2>/dev/null"),
        };

        println!("Waiting for {} to be ready...", wm_name);

        for attempt in 0..timeout_secs {
            let output = self.exec_in_wm(wm_type, check_cmd)?;
            if output.status.success() {
                println!("✓ {} is ready!", wm_name);
                return Ok(());
            }
            if attempt % 5 == 0 && attempt > 0 {
                println!("  Still waiting... ({}/{}s)", attempt, timeout_secs);
            }
            thread::sleep(Duration::from_secs(1));
        }

        anyhow::bail!("{} failed to start within {} seconds", wm_name, timeout_secs)
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

    pub fn copy_from_wm(&self, wm_type: TestWmType, container_path: &str, host_path: &str) -> Result<()> {
        let container_id = self.wm_container(wm_type).id();
        let status = Command::new(runtime().cli)
            .args(&["cp", &format!("{}:{}", container_id, container_path), host_path])
            .status()
            .context("Failed to copy file from container")?;

        if !status.success() {
            anyhow::bail!("Copy failed");
        }
        Ok(())
    }
}
