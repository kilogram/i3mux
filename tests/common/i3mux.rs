// i3mux command wrappers for testing

use anyhow::{Context, Result};
use super::docker::{ContainerManager, TestWmType};
use super::environment::{Session, ColorScript};

pub struct I3muxRunner<'a> {
    container_mgr: &'a ContainerManager,
}

impl<'a> I3muxRunner<'a> {
    pub fn new(container_mgr: &'a ContainerManager) -> Self {
        Self { container_mgr }
    }

    /// Get environment prefix for commands based on WM type
    fn env_prefix(&self) -> &'static str {
        match self.container_mgr.wm_type() {
            TestWmType::I3 => "DISPLAY=:99",
            TestWmType::Sway => "source /tmp/sway-env.sh &&",
        }
    }

    /// Get the WM message command
    fn wm_msg(&self) -> &'static str {
        match self.container_mgr.wm_type() {
            TestWmType::I3 => "i3-msg",
            TestWmType::Sway => "swaymsg",
        }
    }

    /// Get the default terminal for this WM
    fn default_terminal(&self) -> &'static str {
        match self.container_mgr.wm_type() {
            TestWmType::I3 => "xterm",
            TestWmType::Sway => "foot",
        }
    }

    /// Activate i3mux for a workspace
    pub fn activate(&self, session: &Session, workspace: &str) -> Result<()> {
        let env = self.env_prefix();
        let msg = self.wm_msg();
        let term = self.default_terminal();

        let cmd = match session {
            Session::Local => format!(
                "{} {} workspace {} && {} TERMINAL={} i3mux activate",
                env, msg, workspace, env, term
            ),
            Session::Remote(host) => format!(
                "{} {} workspace {} && {} TERMINAL={} i3mux activate --remote {}",
                env, msg, workspace, env, term, host
            ),
        };

        let output = self.container_mgr.exec_in_wm(&cmd)?;

        if !output.status.success() {
            anyhow::bail!(
                "i3mux activate failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    /// Detach current session
    pub fn detach(&self, name: &str) -> Result<()> {
        let cmd = format!("{} i3mux detach --session {}", self.env_prefix(), name);

        let output = self.container_mgr.exec_in_wm(&cmd)?;

        if !output.status.success() {
            anyhow::bail!(
                "i3mux detach failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    /// Attach to a session
    pub fn attach(&self, session: &Session, name: &str, force: bool) -> Result<()> {
        let force_flag = if force { "--force" } else { "" };
        let env = self.env_prefix();
        let term = self.default_terminal();

        let cmd = match session {
            Session::Local => format!("{} TERMINAL={} i3mux attach {} --session {}", env, term, force_flag, name),
            Session::Remote(host) => format!(
                "{} TERMINAL={} i3mux attach --remote {} {} --session {}",
                env, term, host, force_flag, name
            ),
        };

        let output = self.container_mgr.exec_in_wm(&cmd)?;

        if !output.status.success() {
            anyhow::bail!(
                "i3mux attach failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    /// Launch a terminal with a color script
    pub fn launch_terminal(&self, color: &ColorScript) -> Result<u64> {
        let color_code = match color {
            ColorScript::Red => 41,
            ColorScript::Green => 42,
            ColorScript::Blue => 44,
            ColorScript::Yellow => 43,
            ColorScript::Magenta => 45,
            ColorScript::Cyan => 46,
        };

        let env = self.env_prefix();
        let term_exec = match self.container_mgr.wm_type() {
            TestWmType::I3 => "xterm -e",
            TestWmType::Sway => "foot",  // foot doesn't need -e for direct command
        };

        let cmd = format!(
            "{} TERMINAL='{}' i3mux terminal -- /opt/i3mux-test/color-scripts/color-fill.sh {} solid",
            env, term_exec, color_code
        );

        // Get window count before launch
        let before = self.get_window_count()?;

        // Launch terminal
        let output = self.container_mgr.exec_in_wm(&cmd)?;

        if !output.status.success() {
            anyhow::bail!(
                "i3mux terminal failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Wait for window to appear
        for _ in 0..30 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let after = self.get_window_count()?;
            if after > before {
                return self.get_focused_container_id();
            }
        }

        anyhow::bail!("Terminal window did not appear within timeout")
    }

    /// List sessions (kept for potential future session management tests)
    #[allow(dead_code)]
    pub fn list_sessions(&self, session: &Session) -> Result<Vec<String>> {
        let env = self.env_prefix();
        let cmd = match session {
            Session::Local => format!("{} i3mux list", env),
            Session::Remote(host) => format!("{} i3mux list --remote {}", env, host),
        };

        let output = self.container_mgr.exec_in_wm(&cmd)?;

        if !output.status.success() {
            return Ok(Vec::new());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let sessions: Vec<String> = stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.trim().to_string())
            .collect();

        Ok(sessions)
    }

    /// Kill a session (kept for potential future session management tests)
    #[allow(dead_code)]
    pub fn kill_session(&self, session: &Session, name: &str) -> Result<()> {
        let env = self.env_prefix();
        let cmd = match session {
            Session::Local => format!("{} i3mux kill {}", env, name),
            Session::Remote(host) => format!("{} i3mux kill --remote {} {}", env, host, name),
        };

        let output = self.container_mgr.exec_in_wm(&cmd)?;

        if !output.status.success() {
            anyhow::bail!(
                "i3mux kill failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    /// Get number of windows/containers in current workspace
    fn get_window_count(&self) -> Result<usize> {
        // Count containers with app_id (Sway) or window (i3) properties
        let cmd = format!(
            "{} {} -t get_tree | grep -c -E '\"(app_id|window)\"'",
            self.env_prefix(),
            self.wm_msg()
        );
        let output = self.container_mgr.exec_in_wm(&cmd)?;

        let count_str = String::from_utf8_lossy(&output.stdout);
        let count = count_str.trim().parse().unwrap_or(0);

        Ok(count)
    }

    /// Get focused container ID
    fn get_focused_container_id(&self) -> Result<u64> {
        // Use container ID (works for both i3 and Sway)
        let cmd = format!(
            "{} {} -t get_tree | jq -r '.. | select(.focused? == true and (.app_id? != null or .window? != null)) | .id' | head -1",
            self.env_prefix(),
            self.wm_msg()
        );
        let output = self.container_mgr.exec_in_wm(&cmd)?;

        let id_str = String::from_utf8_lossy(&output.stdout);
        let id = id_str
            .trim()
            .parse()
            .context("Failed to parse container ID")?;

        Ok(id)
    }
}
