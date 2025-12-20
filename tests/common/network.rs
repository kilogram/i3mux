// Network manipulation for SSH failure simulation
// Some methods are kept for potential future network failure tests

use anyhow::Result;
use super::docker::ContainerManager;

#[allow(dead_code)]
pub struct NetworkManipulator<'a> {
    container_mgr: &'a ContainerManager,
}

#[allow(dead_code)]
impl<'a> NetworkManipulator<'a> {
    pub fn new(container_mgr: &'a ContainerManager) -> Self {
        Self { container_mgr }
    }

    /// Inject network latency (in milliseconds) with optional jitter
    pub fn inject_latency(&self, latency_ms: u32, jitter_ms: u32) -> Result<()> {
        let cmd = if jitter_ms > 0 {
            format!(
                "sudo tc qdisc add dev eth0 root netem delay {}ms {}ms",
                latency_ms, jitter_ms
            )
        } else {
            format!("sudo tc qdisc add dev eth0 root netem delay {}ms", latency_ms)
        };

        let output = self.container_mgr.exec_in_remote(&cmd)?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to inject latency: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        println!("Injected {}ms latency (±{}ms jitter)", latency_ms, jitter_ms);
        Ok(())
    }

    /// Inject packet loss (percentage)
    pub fn inject_packet_loss(&self, percentage: u32) -> Result<()> {
        if percentage > 100 {
            anyhow::bail!("Packet loss percentage must be <= 100");
        }

        let cmd = format!("sudo tc qdisc add dev eth0 root netem loss {}%", percentage);

        let output = self.container_mgr.exec_in_remote(&cmd)?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to inject packet loss: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        println!("Injected {}% packet loss", percentage);
        Ok(())
    }

    /// Inject bandwidth throttling (in KB/s)
    pub fn inject_bandwidth_limit(&self, kbps: u32) -> Result<()> {
        let cmd = format!(
            "sudo tc qdisc add dev eth0 root tbf rate {}kbit burst 32kbit latency 400ms",
            kbps * 8 // Convert KB/s to kbit/s
        );

        let output = self.container_mgr.exec_in_remote(&cmd)?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to inject bandwidth limit: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        println!("Limited bandwidth to {} KB/s", kbps);
        Ok(())
    }

    /// Drop all SSH connections
    pub fn drop_ssh_connections(&self) -> Result<()> {
        let cmd = "sudo iptables -A INPUT -p tcp --dport 22 -j DROP && \
                   sudo iptables -A INPUT -m state --state ESTABLISHED -j DROP";

        let output = self.container_mgr.exec_in_remote(cmd)?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to drop SSH connections: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        println!("Dropped all SSH connections");
        Ok(())
    }

    /// Block DNS resolution
    pub fn block_dns(&self) -> Result<()> {
        let cmd = "sudo iptables -A OUTPUT -p udp --dport 53 -j DROP && \
                   sudo iptables -A OUTPUT -p tcp --dport 53 -j DROP";

        let output = self.container_mgr.exec_in_remote(cmd)?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to block DNS: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        println!("Blocked DNS resolution");
        Ok(())
    }

    /// Restart SSH daemon
    pub fn restart_sshd(&self) -> Result<()> {
        let cmd = "sudo systemctl restart sshd || sudo service ssh restart";

        let output = self.container_mgr.exec_in_remote(cmd)?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to restart sshd: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        println!("Restarted SSH daemon");
        Ok(())
    }

    /// Clear all network rules (tc and iptables)
    pub fn clear_all_rules(&self) -> Result<()> {
        // Clear tc rules
        let _ = self.container_mgr.exec_in_remote("sudo tc qdisc del dev eth0 root 2>/dev/null");

        // Clear iptables rules
        let _ = self.container_mgr.exec_in_remote("sudo iptables -F");
        let _ = self.container_mgr.exec_in_remote("sudo iptables -X");

        println!("Cleared all network manipulation rules");
        Ok(())
    }
}
