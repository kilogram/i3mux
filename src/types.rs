//! Type-safe wrappers for validated user input.
//!
//! All user input is validated at the CLI boundary and wrapped in these types.
//! Internal code can trust that these values are safe to use in shell commands.

use anyhow::Result;

/// A validated session name.
///
/// Only contains alphanumeric characters, hyphens, and underscores.
/// Safe to use in shell commands without escaping.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionName(String);

impl SessionName {
    /// Creates a new SessionName after validation.
    ///
    /// # Errors
    /// Returns error if the name is empty or contains invalid characters.
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();

        if name.is_empty() {
            anyhow::bail!("Session name cannot be empty");
        }

        if !name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
            anyhow::bail!(
                "Invalid session name '{}': only alphanumeric characters, hyphens, and underscores are allowed",
                name
            );
        }

        Ok(Self(name))
    }

    /// Returns the session name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for SessionName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// A validated remote host identifier.
///
/// Represents a remote SSH host. Can be either:
/// - A hostname (alphanumeric, hyphens, dots)
/// - user@hostname format
///
/// Safe to use in SSH commands.
/// Note: Local connections are represented by `None`, not a RemoteHost.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemoteHost(String);

impl RemoteHost {
    /// Creates a new RemoteHost after validation.
    ///
    /// # Errors
    /// Returns error if the host string is invalid.
    pub fn new(host: impl Into<String>) -> Result<Self> {
        let host = host.into();

        if host.is_empty() {
            anyhow::bail!("Remote host cannot be empty");
        }

        // Split on @ if present
        let (user_part, host_part) = if let Some(idx) = host.find('@') {
            (Some(&host[..idx]), &host[idx + 1..])
        } else {
            (None, host.as_str())
        };

        // Validate username if present
        if let Some(user) = user_part {
            if user.is_empty() {
                anyhow::bail!("Username cannot be empty in '{}'", host);
            }
            if !user.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
                anyhow::bail!(
                    "Invalid username in '{}': only alphanumeric, hyphens, and underscores allowed",
                    host
                );
            }
        }

        // Validate hostname
        if host_part.is_empty() {
            anyhow::bail!("Hostname cannot be empty");
        }

        if !host_part.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '.' || c == '_') {
            anyhow::bail!(
                "Invalid hostname in '{}': only alphanumeric, hyphens, dots, and underscores allowed",
                host
            );
        }

        Ok(Self(host))
    }

    /// Returns the host string as a slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RemoteHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for RemoteHost {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_session_names() {
        assert!(SessionName::new("my-session").is_ok());
        assert!(SessionName::new("session_123").is_ok());
        assert!(SessionName::new("abc123").is_ok());
    }

    #[test]
    fn test_invalid_session_names() {
        assert!(SessionName::new("").is_err());
        assert!(SessionName::new("my session").is_err()); // Space
        assert!(SessionName::new("my/session").is_err()); // Slash
        assert!(SessionName::new("my;session").is_err()); // Semicolon
    }

    #[test]
    fn test_valid_remote_hosts() {
        assert!(RemoteHost::new("local").is_ok());
        assert!(RemoteHost::new("myserver").is_ok());
        assert!(RemoteHost::new("server.example.com").is_ok());
        assert!(RemoteHost::new("user@server").is_ok());
        assert!(RemoteHost::new("user@server.example.com").is_ok());
    }

    #[test]
    fn test_invalid_remote_hosts() {
        assert!(RemoteHost::new("").is_err());
        assert!(RemoteHost::new("@server").is_err()); // Empty username
        assert!(RemoteHost::new("user@").is_err()); // Empty hostname
        assert!(RemoteHost::new("user name@server").is_err()); // Space in username
    }

    // TODO: Implement is_local() method
    // #[test]
    // fn test_remote_host_is_local() {
    //     let local = RemoteHost::new("local").unwrap();
    //     assert!(local.is_local());
    //
    //     let remote = RemoteHost::new("server").unwrap();
    //     assert!(!remote.is_local());
    // }
}
