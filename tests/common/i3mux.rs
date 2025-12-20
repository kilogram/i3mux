// i3mux command wrappers for testing

use anyhow::{Context, Result};
use super::docker::ContainerManager;
use super::environment::{Session, ColorScript};

pub struct I3muxRunner<'a> {
    container_mgr: &'a ContainerManager,
}

impl<'a> I3muxRunner<'a> {
    pub fn new(container_mgr: &'a ContainerManager) -> Self {
        Self { container_mgr }
    }

    /// Activate i3mux for a workspace
    pub fn activate(&self, session: &Session, workspace: &str) -> Result<()> {
        let cmd = match session {
            Session::Local => format!(
                "DISPLAY=:99 i3-msg workspace {} && DISPLAY=:99 TERMINAL=xterm i3mux activate",
                workspace
            ),
            Session::Remote(host) => format!(
                "DISPLAY=:99 i3-msg workspace {} && DISPLAY=:99 TERMINAL=xterm i3mux activate --remote {}",
                workspace, host
            ),
        };

        let output = self.container_mgr.exec_in_xephyr(&cmd)?;

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
        let cmd = format!("DISPLAY=:99 i3mux detach --session {}", name);

        let output = self.container_mgr.exec_in_xephyr(&cmd)?;

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

        let cmd = match session {
            Session::Local => format!("DISPLAY=:99 TERMINAL=xterm i3mux attach {} --session {}", force_flag, name),
            Session::Remote(host) => format!(
                "DISPLAY=:99 TERMINAL=xterm i3mux attach --remote {} {} --session {}",
                host, force_flag, name
            ),
        };

        let output = self.container_mgr.exec_in_xephyr(&cmd)?;

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

        let cmd = format!(
            "DISPLAY=:99 TERMINAL='xterm -e' i3mux terminal -- /opt/i3mux-test/color-scripts/color-fill.sh {} solid",
            color_code
        );

        // Get window count before launch
        let before = self.get_window_count()?;

        // Launch terminal
        let output = self.container_mgr.exec_in_xephyr(&cmd)?;

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
                return self.get_focused_window_id();
            }
        }

        anyhow::bail!("Terminal window did not appear within timeout")
    }

    /// List sessions (kept for potential future session management tests)
    #[allow(dead_code)]
    pub fn list_sessions(&self, session: &Session) -> Result<Vec<String>> {
        let cmd = match session {
            Session::Local => "DISPLAY=:99 i3mux list".to_string(),
            Session::Remote(host) => format!("DISPLAY=:99 i3mux list --remote {}", host),
        };

        let output = self.container_mgr.exec_in_xephyr(&cmd)?;

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
        let cmd = match session {
            Session::Local => format!("DISPLAY=:99 i3mux kill {}", name),
            Session::Remote(host) => format!("DISPLAY=:99 i3mux kill --remote {} {}", host, name),
        };

        let output = self.container_mgr.exec_in_xephyr(&cmd)?;

        if !output.status.success() {
            anyhow::bail!(
                "i3mux kill failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    /// Get number of windows in current workspace
    fn get_window_count(&self) -> Result<usize> {
        let output = self.container_mgr.exec_in_xephyr(
            "DISPLAY=:99 i3-msg -t get_tree | grep -c '\"window\"'"
        )?;

        let count_str = String::from_utf8_lossy(&output.stdout);
        let count = count_str.trim().parse().unwrap_or(0);

        Ok(count)
    }

    /// Get focused window ID
    fn get_focused_window_id(&self) -> Result<u64> {
        let output = self.container_mgr.exec_in_xephyr(
            "DISPLAY=:99 i3-msg -t get_tree | grep -Po '\"focused\":true.*?\"window\":\\K[0-9]+' | head -1"
        )?;

        let id_str = String::from_utf8_lossy(&output.stdout);
        let id = id_str
            .trim()
            .parse()
            .context("Failed to parse window ID")?;

        Ok(id)
    }
}
