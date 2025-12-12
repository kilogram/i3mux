// Container management using testcontainers-rs (v0.23 API)

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration;
use testcontainers::{core::WaitFor, runners::SyncRunner, GenericImage, ImageExt};

pub struct ContainerManager {
    xvfb_container: testcontainers::Container<GenericImage>,
    remote_container: testcontainers::Container<GenericImage>,
}

impl ContainerManager {
    pub fn new() -> Result<Self> {
        // Build images ONCE (they'll be cached by docker/podman)
        Self::ensure_images_built()?;

        println!("Starting containers with testcontainers...");

        // Create Xvfb container - testcontainers handles lifecycle
        let xvfb_image = GenericImage::new("localhost/docker_i3mux-test-xephyr", "latest")
            .with_wait_for(WaitFor::Nothing);

        let xvfb_container = xvfb_image.start()?;

        // Create SSH remote container
        let remote_image = GenericImage::new("localhost/docker_i3mux-remote-ssh", "latest")
            .with_wait_for(WaitFor::Nothing);

        let remote_container = remote_image.start()?;

        println!("✓ Containers started (testcontainers will auto-cleanup)");

        let mgr = Self {
            xvfb_container,
            remote_container,
        };

        // Copy i3mux binary and test scripts into containers
        mgr.setup_container_files()?;

        Ok(mgr)
    }

    fn setup_container_files(&self) -> Result<()> {
        let docker_cmd = Self::get_docker_cmd();
        let manifest_dir = env!("CARGO_MANIFEST_DIR");

        // Copy i3mux binary to xvfb container (use statically-linked musl binary)
        let i3mux_binary = PathBuf::from(manifest_dir).join("target/x86_64-unknown-linux-musl/debug/i3mux");
        if !i3mux_binary.exists() {
            anyhow::bail!("i3mux binary not found. Please run 'cargo build --target x86_64-unknown-linux-musl' first.");
        }

        let xvfb_id = self.xvfb_container.id();
        Command::new(&docker_cmd)
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
        Command::new(&docker_cmd)
            .args(&[
                "cp",
                color_fill_script.to_str().unwrap(),
                &format!("{}:/opt/i3mux-test/color-scripts/color-fill.sh", xvfb_id),
            ])
            .status()
            .context("Failed to copy color-fill.sh to xvfb container")?;

        self.exec_in_xephyr("chmod +x /opt/i3mux-test/color-scripts/color-fill.sh")?;

        Ok(())
    }

    fn ensure_images_built() -> Result<()> {
        // Check if images exist
        let docker_cmd = Self::get_docker_cmd();
        let check = Command::new(&docker_cmd)
            .args(&["images", "-q", "localhost/docker_i3mux-test-xephyr:latest"])
            .output()?;

        if check.stdout.is_empty() {
            // Images don't exist, build them
            println!("Building container images (one-time setup)...");
            let docker_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/docker");

            let compose_cmd = if Command::new("podman-compose").arg("--version").output().is_ok() {
                "podman-compose"
            } else {
                "docker-compose"
            };

            let status = Command::new(compose_cmd)
                .current_dir(&docker_dir)
                .args(&["build"])
                .status()
                .context("Failed to build images")?;

            if !status.success() {
                anyhow::bail!("Image build failed");
            }
            println!("✓ Images built and cached");
        } else {
            println!("✓ Using cached container images");
        }

        Ok(())
    }

    fn get_docker_cmd() -> String {
        if Command::new("podman").arg("--version").output().is_ok() {
            "podman".to_string()
        } else {
            "docker".to_string()
        }
    }

    pub fn exec_in_xephyr(&self, cmd: &str) -> Result<std::process::Output> {
        let container_id = self.xvfb_container.id();
        let docker_cmd = Self::get_docker_cmd();

        Command::new(docker_cmd)
            .args(&["exec", container_id, "bash", "-c", cmd])
            .output()
            .context("Failed to exec in Xvfb container")
    }

    pub fn exec_in_remote(&self, cmd: &str) -> Result<std::process::Output> {
        let container_id = self.remote_container.id();
        let docker_cmd = Self::get_docker_cmd();

        Command::new(docker_cmd)
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
        let docker_cmd = Self::get_docker_cmd();

        let status = Command::new(docker_cmd)
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
