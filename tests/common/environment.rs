// Test environment management - orchestrates containers, i3mux, and screenshots

use anyhow::{Context, Result};
use image::RgbaImage;
use std::path::PathBuf;
use std::time::Duration;

use super::docker::{ContainerManager, TestWmType};
use super::i3mux::I3muxRunner;
use super::network::NetworkManipulator;
use super::screenshot::{compare_screenshots, load_golden_image, save_comparison_failure};
use super::comparison_spec::ComparisonSpec;

/// Session type for i3mux
#[derive(Debug, Clone)]
pub enum Session {
    Local,
    Remote(&'static str),
}

/// Color for terminal backgrounds (used by network failure tests)
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
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
        container_mgr.wait_for_wm_ready(30)?;
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

    /// Launch a terminal with colored background (used by network tests)
    pub fn launch_terminal(&self, color: ColorScript) -> Result<u64> {
        self.i3mux().launch_terminal(&color)
    }

    /// Launch a terminal running a command (WM-agnostic)
    /// Used for tests that need to spawn non-i3mux terminals
    pub fn launch_terminal_with_command(&self, command: &str) -> Result<()> {
        let (terminal_cmd, env_prefix) = match self.container_mgr.wm_type() {
            TestWmType::I3 => ("xterm -e", "DISPLAY=:99"),
            TestWmType::Sway => ("foot", "source /tmp/sway-env.sh &&"),
        };

        let cmd = format!(
            "{} {} 'exec --no-startup-id {} {}'",
            env_prefix,
            match self.container_mgr.wm_type() {
                TestWmType::I3 => "i3-msg",
                TestWmType::Sway => "swaymsg",
            },
            terminal_cmd,
            command
        );
        let output = self.container_mgr.exec_in_wm(&cmd)?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to launch terminal: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    // ==================== Window Manager Operations ====================

    /// Get the WM-specific message command prefix
    fn wm_cmd_prefix(&self) -> &'static str {
        match self.container_mgr.wm_type() {
            TestWmType::I3 => "DISPLAY=:99 i3-msg",
            TestWmType::Sway => "source /tmp/sway-env.sh && swaymsg",
        }
    }

    /// Execute a WM command (i3-msg or swaymsg)
    pub fn wm_exec(&self, cmd: &str) -> Result<()> {
        let full_cmd = format!("{} '{}'", self.wm_cmd_prefix(), cmd);
        let output = self.container_mgr.exec_in_wm(&full_cmd)?;

        if !output.status.success() {
            anyhow::bail!(
                "WM command failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    /// Execute an i3 command (alias for wm_exec for backward compatibility)
    pub fn i3_exec(&self, cmd: &str) -> Result<()> {
        self.wm_exec(cmd)
    }

    /// Check if workspace is empty
    pub fn is_workspace_empty(&self, workspace: &str) -> Result<bool> {
        let cmd = format!(
            "{} -t get_tree | grep -q 'workspace \"{}\"' && echo found || echo empty",
            self.wm_cmd_prefix(),
            workspace
        );

        let output = self.container_mgr.exec_in_wm(&cmd)?;
        let result = String::from_utf8_lossy(&output.stdout);

        Ok(result.contains("empty"))
    }

    /// Get list of container IDs in current workspace
    /// Note: For Sway we use container IDs; for i3 we traditionally used X11 window IDs
    /// but con_id works for both, so we use container IDs universally now
    pub fn get_workspace_windows(&self) -> Result<Vec<u64>> {
        // Get focused workspace number first
        let ws_cmd = format!(
            "{} -t get_workspaces | jq -r '.[] | select(.focused==true) | .num'",
            self.wm_cmd_prefix()
        );
        let ws_output = self.container_mgr.exec_in_wm(&ws_cmd)?;

        let ws_num = String::from_utf8_lossy(&ws_output.stdout)
            .trim()
            .parse::<i32>()
            .context("Failed to get focused workspace number")?;

        // Get container IDs in that workspace
        // Use 'id' field which is the container ID (works for both i3 and Sway)
        let tree_cmd = format!(
            r#"{} -t get_tree | jq -r '.. | select(.type? == "workspace" and .num? == {}) | .. | select(.id? != null and (.app_id? != null or .window_properties? != null)) | .id'"#,
            self.wm_cmd_prefix(),
            ws_num
        );
        let output = self.container_mgr.exec_in_wm(&tree_cmd)?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let windows: Vec<u64> = stdout
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect();

        Ok(windows)
    }

    pub fn get_window_info(&self, container_id: u64) -> Result<String> {
        let cmd = format!(
            r#"{} -t get_tree | jq -r '.. | select(.id? == {}) | {{name: .name, marks: .marks, app_id: .app_id}}'"#,
            self.wm_cmd_prefix(),
            container_id
        );
        let output = self.container_mgr.exec_in_wm(&cmd)?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Launch an i3mux terminal and wait for it to appear
    pub fn launch_i3mux_terminal(&self) -> Result<()> {
        // Get window count before launch
        let before = self.get_workspace_windows()?.len();

        // Set up appropriate terminal and env vars based on WM type
        let (terminal, env_prefix, msg_cmd) = match self.container_mgr.wm_type() {
            TestWmType::I3 => ("xterm", "DISPLAY=:99", "i3-msg"),
            TestWmType::Sway => ("foot", "source /tmp/sway-env.sh &&", "swaymsg"),
        };

        // Launch via WM exec so WM spawns the process
        let launch_cmd = format!(
            "{} {} 'exec --no-startup-id TERMINAL={} i3mux terminal 2>>/tmp/i3mux-debug.log'",
            env_prefix,
            msg_cmd,
            terminal
        );
        let output = self.container_mgr.exec_in_wm(&launch_cmd)?;

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

    /// Capture a screenshot of the display (Xephyr for i3, headless for Sway)
    pub fn capture_screenshot(&self) -> Result<RgbaImage> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis();

        let screenshot_path = format!("/tmp/screenshots/test-{}.png", timestamp);

        // Ensure screenshots directory exists
        self.container_mgr.exec_in_wm("mkdir -p /tmp/screenshots")?;

        // Capture screenshot using appropriate tool
        let cmd = match self.container_mgr.wm_type() {
            TestWmType::I3 => format!("DISPLAY=:99 scrot -o {}", screenshot_path),
            TestWmType::Sway => format!(
                "source /tmp/sway-env.sh && grim {}",
                screenshot_path
            ),
        };
        let output = self.container_mgr.exec_in_wm(&cmd)?;

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

        self.container_mgr.copy_from_wm(&screenshot_path, &host_path)?;

        // Load and return image
        let img = image::open(&host_path)
            .context("Failed to open screenshot")?
            .to_rgba8();

        // Clean up temporary file
        let _ = std::fs::remove_file(&host_path);

        Ok(img)
    }

    /// Get the WM-specific golden image subdirectory
    fn golden_subdir(&self) -> &'static str {
        match self.container_mgr.wm_type() {
            TestWmType::I3 => "i3",
            TestWmType::Sway => "sway",
        }
    }

    /// Compare screenshot with golden image
    pub fn compare_with_golden(
        &self,
        golden_name: &str,
        actual: &RgbaImage,
        spec: &ComparisonSpec,
    ) -> Result<()> {
        // Use WM-specific golden image path
        let golden_subpath = format!("{}/{}", self.golden_subdir(), golden_name);

        if self.update_goldens {
            // Save as golden image in WM-specific subdirectory
            let golden_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/integration/golden")
                .join(&golden_subpath);

            if let Some(parent) = golden_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            actual.save(&golden_path)?;
            println!("  ✓ Updated golden image: {}", golden_subpath);
            return Ok(());
        }

        // Normal comparison mode - load from WM-specific subdirectory
        let golden = load_golden_image(&golden_subpath)?;

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

    /// Focus the next tab/window in a tabbed container (horizontal cycling)
    pub fn focus_next_tab(&self) -> Result<()> {
        self.i3_exec("focus right")
    }

    /// Focus the previous tab/window in a tabbed container (horizontal cycling)
    pub fn focus_prev_tab(&self) -> Result<()> {
        self.i3_exec("focus left")
    }

    /// Focus the next window in a stacked container (vertical cycling)
    pub fn focus_next_stack(&self) -> Result<()> {
        self.i3_exec("focus down")
    }

    /// Focus the previous window in a stacked container (vertical cycling)
    pub fn focus_prev_stack(&self) -> Result<()> {
        self.i3_exec("focus up")
    }

    /// Capture multiple screenshots by cycling through tabs/stacks
    /// This navigates through all visible containers using focus commands
    /// and captures a screenshot at each position.
    ///
    /// `count` - number of screenshots to take (should match number of tabs/stack items)
    /// `direction` - "next" to cycle right/down, "prev" to cycle left/up
    pub fn capture_multi_screenshots(&self, count: usize, direction: &str) -> Result<Vec<RgbaImage>> {
        let mut screenshots = Vec::with_capacity(count);

        for i in 0..count {
            // Capture current screenshot
            let screenshot = self.capture_screenshot()?;
            screenshots.push(screenshot);

            // Navigate to next tab/stack (except for last one)
            if i < count - 1 {
                std::thread::sleep(Duration::from_millis(100));
                if direction == "next" {
                    self.focus_next_tab()?;
                } else {
                    self.focus_prev_tab()?;
                }
                std::thread::sleep(Duration::from_millis(300)); // Wait for focus change to render
            }
        }

        Ok(screenshots)
    }

    /// Compare multiple screenshots with corresponding golden images
    /// Golden images are named with suffix like "base-name-1.png", "base-name-2.png", etc.
    pub fn compare_multi_with_golden(
        &self,
        golden_base_name: &str,
        screenshots: &[RgbaImage],
        spec: &ComparisonSpec,
    ) -> Result<()> {
        for (i, screenshot) in screenshots.iter().enumerate() {
            let golden_name = format!("{}-{}.png", golden_base_name, i + 1);
            self.compare_with_golden(&golden_name, screenshot, spec)?;
        }
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

    /// Clear all network manipulation rules
    pub fn clear_network_rules(&self) -> Result<()> {
        self.network().clear_all_rules()
    }

    // ==================== Debug Helpers ====================

    /// Read i3mux debug log from container
    pub fn read_debug_log(&self) -> Result<String> {
        let output = self.container_mgr.exec_in_wm("cat /tmp/i3mux-debug.log 2>/dev/null || echo 'No debug log'")?;
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
