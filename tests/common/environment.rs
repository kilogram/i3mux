// Test environment management - orchestrates containers, i3mux, and screenshots

use anyhow::{Context, Result};
use image::RgbaImage;
use std::path::PathBuf;
use std::time::Duration;

use super::docker::ContainerManager;
use super::i3mux::I3muxRunner;
use super::network::NetworkManipulator;
use super::screenshot::{compare_screenshots, load_golden_image, save_comparison_failure, ComparisonResult};
use super::comparison_spec::ComparisonSpec;

/// Session type for i3mux
#[derive(Debug, Clone)]
pub enum Session {
    Local,
    Remote(&'static str),
}

/// Color for terminal backgrounds
#[derive(Debug, Clone, Copy)]
pub enum ColorScript {
    Red,
    Green,
    Blue,
    Yellow,
    Magenta,
    Cyan,
}

/// Main test environment managing containers and test operations
pub struct TestEnvironment {
    container_mgr: ContainerManager,
    update_goldens: bool,
}

impl TestEnvironment {
    /// Create a new test environment
    /// Creates fresh containers for this test session
    /// Docker images are cached and reused automatically
    pub fn new() -> Result<Self> {
        println!("\n=== Creating test environment ===");

        let container_mgr = ContainerManager::new()
            .context("Failed to create container manager")?;

        println!("=== Waiting for services to be ready ===");
        container_mgr.wait_for_xephyr_ready(30)?;
        container_mgr.wait_for_ssh_ready(30)?;
        println!("=== Test environment ready ===\n");

        // Check for UPDATE_GOLDENS environment variable
        let update_goldens = std::env::var("UPDATE_GOLDENS").is_ok();
        if update_goldens {
            println!("NOTE: Running in golden update mode - will regenerate golden images");
        }

        Ok(Self {
            container_mgr,
            update_goldens,
        })
    }

    /// Get reference to i3mux runner
    fn i3mux(&self) -> I3muxRunner<'_> {
        I3muxRunner::new(&self.container_mgr)
    }

    /// Get reference to network manipulator
    fn network(&self) -> NetworkManipulator<'_> {
        NetworkManipulator::new(&self.container_mgr)
    }

    // ==================== i3mux Operations ====================

    /// Activate i3mux for a workspace
    pub fn i3mux_activate(&self, session: Session, workspace: &str) -> Result<()> {
        self.i3mux().activate(&session, workspace)
    }

    /// Detach current session
    pub fn i3mux_detach(&self, name: &str) -> Result<()> {
        self.i3mux().detach(name)
    }

    /// Attach to a session
    pub fn i3mux_attach(&self, session: Session, name: &str) -> Result<()> {
        self.i3mux().attach(&session, name, false)
    }

    /// Attach to a session with --force
    pub fn i3mux_attach_force(&self, session: Session, name: &str) -> Result<()> {
        self.i3mux().attach(&session, name, true)
    }

    /// Launch a terminal with colored background
    pub fn launch_terminal(&self, color: ColorScript) -> Result<u64> {
        self.i3mux().launch_terminal(&color)
    }

    /// List available sessions
    pub fn list_sessions(&self, session: Session) -> Result<Vec<String>> {
        self.i3mux().list_sessions(&session)
    }

    /// Kill a session
    pub fn kill_session(&self, session: Session, name: &str) -> Result<()> {
        self.i3mux().kill_session(&session, name)
    }

    // ==================== i3 Window Manager Operations ====================

    /// Execute an i3 command
    pub fn i3_exec(&self, cmd: &str) -> Result<()> {
        let full_cmd = format!("DISPLAY=:99 i3-msg '{}'", cmd);
        let output = self.container_mgr.exec_in_xephyr(&full_cmd)?;

        if !output.status.success() {
            anyhow::bail!(
                "i3 command failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    /// Check if workspace is empty
    pub fn is_workspace_empty(&self, workspace: &str) -> Result<bool> {
        let cmd = format!(
            "DISPLAY=:99 i3-msg -t get_tree | grep -q 'workspace \"{}\"' && echo found || echo empty",
            workspace
        );

        let output = self.container_mgr.exec_in_xephyr(&cmd)?;
        let result = String::from_utf8_lossy(&output.stdout);

        Ok(result.contains("empty"))
    }

    /// Get list of window IDs in current workspace
    pub fn get_workspace_windows(&self) -> Result<Vec<u64>> {
        // Get focused workspace number first
        let ws_output = self.container_mgr.exec_in_xephyr(
            "DISPLAY=:99 i3-msg -t get_workspaces | jq -r '.[] | select(.focused==true) | .num'"
        )?;

        let ws_num = String::from_utf8_lossy(&ws_output.stdout)
            .trim()
            .parse::<i32>()
            .context("Failed to get focused workspace number")?;

        // Get windows in that workspace
        let output = self.container_mgr.exec_in_xephyr(
            &format!(r#"DISPLAY=:99 i3-msg -t get_tree | jq -r '.. | select(.type? == "workspace" and .num? == {}) | .. | select(.window? != null and .window? != 0) | .window'"#, ws_num)
        )?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let windows: Vec<u64> = stdout
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect();

        Ok(windows)
    }

    pub fn get_window_info(&self, window_id: u64) -> Result<String> {
        let output = self.container_mgr.exec_in_xephyr(&format!(
            r#"DISPLAY=:99 i3-msg -t get_tree | jq -r '.. | select(.window? == {}) | {{name: .name, marks: .marks}}'"#,
            window_id
        ))?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Launch an i3mux terminal and wait for it to appear
    pub fn launch_i3mux_terminal(&self) -> Result<()> {
        // Get window count before launch
        let before = self.get_workspace_windows()?.len();

        // Launch via i3-msg exec so i3 spawns the process (better for i3 integration)
        // Run i3mux in foreground and capture any errors to /tmp/i3mux-debug.log
        let output = self.container_mgr.exec_in_xephyr(
            "DISPLAY=:99 i3-msg 'exec --no-startup-id i3mux terminal 2>>/tmp/i3mux-debug.log'"
        )?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to launch i3mux terminal: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Wait for window to appear (SSH connections can be slow)
        for _ in 0..50 {  // Up to 5 seconds
            std::thread::sleep(Duration::from_millis(100));
            let after = self.get_workspace_windows()?.len();
            if after > before {
                // Window appeared - now wait for i3mux to finish marking
                // This is critical: i3mux needs time to mark the window
                std::thread::sleep(Duration::from_millis(2500));

                // Verify marking succeeded
                let windows = self.get_workspace_windows()?;
                if let Some(new_window) = windows.get(windows.len() - 1) {
                    let info = self.get_window_info(*new_window)?;
                    println!("New window {} info after launch: {}", new_window, info);
                }

                return Ok(());
            }
        }

        anyhow::bail!("i3mux terminal window did not appear within timeout")
    }

    // ==================== Screenshot Operations ====================

    /// Capture a screenshot of the Xephyr display
    pub fn capture_screenshot(&self) -> Result<RgbaImage> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis();

        let screenshot_path = format!("/tmp/screenshots/test-{}.png", timestamp);

        // Ensure screenshots directory exists
        self.container_mgr.exec_in_xephyr("mkdir -p /tmp/screenshots")?;

        // Capture screenshot
        let cmd = format!("DISPLAY=:99 scrot -o {}", screenshot_path);
        let output = self.container_mgr.exec_in_xephyr(&cmd)?;

        if !output.status.success() {
            anyhow::bail!(
                "Screenshot capture failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Copy screenshot to host
        let host_path = format!(
            "{}/tests/screenshots/temp-{}.png",
            env!("CARGO_MANIFEST_DIR"),
            timestamp
        );

        // Ensure host screenshots directory exists
        std::fs::create_dir_all(format!("{}/tests/screenshots", env!("CARGO_MANIFEST_DIR")))?;

        self.container_mgr.copy_from_xephyr(&screenshot_path, &host_path)?;

        // Load and return image
        let img = image::open(&host_path)
            .context("Failed to open screenshot")?
            .to_rgba8();

        // Clean up temporary file
        let _ = std::fs::remove_file(&host_path);

        Ok(img)
    }

    /// Compare screenshot with golden image
    pub fn compare_with_golden(
        &self,
        golden_name: &str,
        actual: &RgbaImage,
        spec: &ComparisonSpec,
    ) -> Result<()> {
        if self.update_goldens {
            // Save as golden image
            let golden_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/golden")
                .join(golden_name);

            if let Some(parent) = golden_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            actual.save(&golden_path)?;
            println!("  ✓ Updated golden image: {}", golden_name);
            return Ok(());
        }

        // Normal comparison mode
        let golden = load_golden_image(golden_name)?;

        let result = compare_screenshots(&golden, actual, spec)?;

        if !result.passed {
            let test_name = std::thread::current().name().unwrap_or("unknown").to_string();
            let failure_dir = save_comparison_failure(&test_name, &golden, actual, &result)?;

            anyhow::bail!(
                "Screenshot comparison failed!\n\
                 Diff pixels: {} ({:.2}%)\n\
                 Artifacts saved to: {}",
                result.total_diff_pixels,
                result.diff_percentage,
                failure_dir.display()
            );
        }

        Ok(())
    }

    /// Wait for window to finish rendering
    pub fn wait_for_window_render(&self, _window_id: u64, duration: Duration) -> Result<()> {
        std::thread::sleep(duration);
        Ok(())
    }

    /// Wait for layout restoration to complete
    pub fn wait_for_layout_restore(&self, duration: Duration) -> Result<()> {
        std::thread::sleep(duration);
        Ok(())
    }

    /// Wait for SSH connection to establish
    pub fn wait_for_ssh_connection(&self, _window_id: u64, timeout: Duration) -> Result<()> {
        std::thread::sleep(timeout);
        Ok(())
    }

    // ==================== Network Manipulation ====================

    /// Inject network latency
    pub fn inject_latency(&self, ms: u32, jitter_ms: u32) -> Result<()> {
        self.network().inject_latency(ms, jitter_ms)
    }

    /// Inject packet loss
    pub fn inject_packet_loss(&self, percentage: u32) -> Result<()> {
        self.network().inject_packet_loss(percentage)
    }

    /// Inject bandwidth throttling
    pub fn inject_bandwidth_limit(&self, kbps: u32) -> Result<()> {
        self.network().inject_bandwidth_limit(kbps)
    }

    /// Drop SSH connections
    pub fn drop_ssh_connections(&self) -> Result<()> {
        self.network().drop_ssh_connections()
    }

    /// Restart SSH daemon
    pub fn restart_sshd(&self) -> Result<()> {
        self.network().restart_sshd()
    }

    /// Block DNS resolution
    pub fn block_dns(&self) -> Result<()> {
        self.network().block_dns()
    }

    /// Clear all network manipulation rules
    pub fn clear_network_rules(&self) -> Result<()> {
        self.network().clear_all_rules()
    }

    // ==================== Debug Helpers ====================

    /// Read i3mux debug log from container
    pub fn read_debug_log(&self) -> Result<String> {
        let output = self.container_mgr.exec_in_xephyr("cat /tmp/i3mux-debug.log 2>/dev/null || echo 'No debug log'")?;
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    // ==================== Cleanup ====================

    /// Clean up workspace (kill all windows, reset state)
    pub fn cleanup_workspace(&self, workspace: &str) -> Result<()> {
        // Switch to workspace
        self.i3_exec(&format!("workspace {}", workspace))?;

        // Kill all windows in workspace
        let windows = self.get_workspace_windows()?;
        for _ in windows {
            let _ = self.i3_exec("kill");
            std::thread::sleep(Duration::from_millis(100));
        }

        // Clear network rules
        let _ = self.clear_network_rules();

        Ok(())
    }
}
