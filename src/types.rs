//! Validated identifiers shared by every module.
//!
//! Each type is built only through validation, so code that holds one never
//! checks it again. The guarantees:
//!
//! - [`SessionName`]: non-empty; only alphanumerics, `-` and `_`; never starts
//!   with `-`. Safe as a file name, a shpool argument, and a mark component.
//! - [`RemoteHost`]: `[user@]host`; never empty, never starts with `-` (so ssh
//!   cannot read it as an option), never the literal `local` (reserved for
//!   [`Host::Local`]), never contains `:` (the mark separator).
//! - [`Host`]: its [`Host::label`] parses back to the same value.
//! - [`TerminalName`]: always `{session}-{NNN}`. `Display` and `FromStr`
//!   round-trip, and [`TerminalName::parse`] matches one session exactly
//!   (plan rule 3).
//! - [`WindowTemplate`]: a command line that splits into at least one word
//!   (PLAN-terminal-here decision 2).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A name failed validation. The message names the input and the rule.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidName {
    #[error("invalid session name '{value}': {reason}")]
    Session { value: String, reason: &'static str },
    #[error("invalid remote host '{value}': {reason}")]
    Host { value: String, reason: &'static str },
    #[error("invalid terminal name '{0}': expected {{session}}-{{NNN}}, for example ws4-001")]
    Terminal(String),
    #[error("invalid window command '{value}': {reason}")]
    WindowCommand { value: String, reason: &'static str },
}

/// The name of an i3mux session, for example `ws4`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SessionName(String);

impl SessionName {
    /// Validate `name` as a session name.
    ///
    /// # Errors
    /// [`InvalidName::Session`] if `name` is empty, starts with `-`, or has a
    /// character other than an alphanumeric, `-` or `_`.
    pub fn new(name: impl Into<String>) -> Result<Self, InvalidName> {
        let value = name.into();
        let reason = if value.is_empty() {
            Some("it is empty")
        } else if value.starts_with('-') {
            Some("it starts with '-'")
        } else if !value.chars().all(is_name_char) {
            Some("only letters, digits, '-' and '_' are allowed")
        } else {
            None
        };
        match reason {
            Some(reason) => Err(InvalidName::Session { value, reason }),
            None => Ok(Self(value)),
        }
    }

    /// The default session for a numbered workspace: `ws{number}`.
    #[must_use]
    pub fn for_workspace(number: u32) -> Self {
        // Valid by construction: "ws" followed by ASCII digits.
        Self(format!("ws{number}"))
    }

    /// The name as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '-' || c == '_'
}

impl fmt::Display for SessionName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for SessionName {
    type Error = InvalidName;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<SessionName> for String {
    fn from(name: SessionName) -> Self {
        name.0
    }
}

/// An ssh destination, `host` or `user@host`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemoteHost(String);

impl RemoteHost {
    /// Validate `host` as an ssh destination.
    ///
    /// # Errors
    /// [`InvalidName::Host`] if `host` is empty, starts with `-`, is `local`,
    /// or its user or host part has characters other than alphanumerics,
    /// `-`, `_` (and `.` in the host part).
    pub fn new(host: impl Into<String>) -> Result<Self, InvalidName> {
        let value = host.into();
        match remote_host_problem(&value) {
            Some(reason) => Err(InvalidName::Host { value, reason }),
            None => Ok(Self(value)),
        }
    }

    /// The destination as passed to ssh.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn remote_host_problem(value: &str) -> Option<&'static str> {
    if value.is_empty() {
        return Some("it is empty");
    }
    if value.starts_with('-') {
        return Some("it starts with '-'");
    }
    if value == Host::LOCAL_LABEL {
        return Some("'local' means this machine; omit --remote for a local session");
    }
    let (user, hostname) = match value.split_once('@') {
        Some((user, hostname)) => (Some(user), hostname),
        None => (None, value),
    };
    if let Some(user) = user {
        if user.is_empty() || !user.chars().all(is_name_char) {
            return Some("the user part may only have letters, digits, '-' and '_'");
        }
    }
    if hostname.is_empty() || !hostname.chars().all(|c| is_name_char(c) || c == '.') {
        return Some("the host part may only have letters, digits, '-', '_' and '.'");
    }
    None
}

impl fmt::Display for RemoteHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a session's shells run: this machine, or a remote host over ssh.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Host {
    Local,
    Ssh(RemoteHost),
}

impl Host {
    /// The label of [`Host::Local`]. [`RemoteHost`] rejects it, so labels are
    /// unambiguous.
    pub const LOCAL_LABEL: &'static str = "local";

    /// `Local` when `remote` is `None`, else the validated remote host.
    ///
    /// # Errors
    /// [`InvalidName::Host`] if `remote` is not a valid [`RemoteHost`].
    pub fn from_remote_arg(remote: Option<String>) -> Result<Self, InvalidName> {
        remote.map_or(Ok(Self::Local), |r| RemoteHost::new(r).map(Self::Ssh))
    }

    /// Parse a label produced by [`Host::label`].
    ///
    /// # Errors
    /// [`InvalidName::Host`] if `label` is neither `local` nor a valid remote host.
    pub fn from_label(label: &str) -> Result<Self, InvalidName> {
        if label == Self::LOCAL_LABEL {
            Ok(Self::Local)
        } else {
            RemoteHost::new(label).map(Self::Ssh)
        }
    }

    /// `local`, or the remote host string. Used in window marks and state.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Local => Self::LOCAL_LABEL,
            Self::Ssh(remote) => remote.as_str(),
        }
    }
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

impl TryFrom<String> for Host {
    type Error = InvalidName;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::from_label(&value)
    }
}

impl From<Host> for String {
    fn from(host: Host) -> Self {
        host.label().to_owned()
    }
}

/// One persistent shell of a session: shpool session `{session}-{NNN}`.
///
/// Ordering is by session, then number.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TerminalName {
    session: SessionName,
    number: u32,
}

impl TerminalName {
    /// Terminal `number` of `session`.
    #[must_use]
    pub fn new(session: &SessionName, number: u32) -> Self {
        Self {
            session: session.clone(),
            number,
        }
    }

    /// Parse `text` as a terminal of exactly `session`.
    ///
    /// Strips exactly `{session}-` and requires the rest to be the canonical
    /// number digits, so session `ws4` never matches `ws4-x-001` (plan rule 3).
    #[must_use]
    pub fn parse(session: &SessionName, text: &str) -> Option<Self> {
        let digits = text.strip_prefix(session.as_str())?.strip_prefix('-')?;
        Some(Self::new(session, parse_canonical_number(digits)?))
    }

    /// The session this terminal belongs to.
    #[must_use]
    pub fn session(&self) -> &SessionName {
        &self.session
    }

    /// The terminal number within its session.
    #[must_use]
    pub fn number(&self) -> u32 {
        self.number
    }
}

/// A window command: what each window of a binding runs instead of i3mux's
/// wrapper (PLAN-terminal-here decision 2). The text splits into words as a
/// shell splits them (quotes, backslashes), and no shell runs it. In each
/// word, [`WindowTemplate::SESSION`] becomes the session name and
/// [`WindowTemplate::SHELL`] the shell name; any other text stays as it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WindowTemplate {
    text: String,
    words: Vec<String>,
}

impl WindowTemplate {
    /// Replaced by the session name, for example `ws4`.
    pub const SESSION: &'static str = "{session}";
    /// Replaced by the shell name, for example `ws4-002`.
    pub const SHELL: &'static str = "{shell}";

    /// Validate `text` as a window command.
    ///
    /// # Errors
    /// [`InvalidName::WindowCommand`] if its quotes do not close, or it has
    /// no word.
    pub fn new(text: impl Into<String>) -> Result<Self, InvalidName> {
        let text = text.into();
        let invalid = |reason| InvalidName::WindowCommand {
            value: text.clone(),
            reason,
        };
        let words = shell_words::split(&text).map_err(|_| invalid("a quote does not close"))?;
        if words.is_empty() {
            return Err(invalid("it has no command"));
        }
        Ok(Self { text, words })
    }

    /// The command as given.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The argv of the window of `shell`.
    #[must_use]
    pub fn argv(&self, shell: &TerminalName) -> Vec<String> {
        let (session, shell) = (shell.session().as_str(), shell.to_string());
        self.words
            .iter()
            .map(|word| {
                word.replace(Self::SESSION, session)
                    .replace(Self::SHELL, &shell)
            })
            .collect()
    }
}

impl TryFrom<String> for WindowTemplate {
    type Error = InvalidName;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<WindowTemplate> for String {
    fn from(template: WindowTemplate) -> Self {
        template.text
    }
}

/// Digits as `Display` writes them (`{:03}`), so parsing round-trips.
fn parse_canonical_number(digits: &str) -> Option<u32> {
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let number: u32 = digits.parse().ok()?;
    (format!("{number:03}") == digits).then_some(number)
}

impl fmt::Display for TerminalName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{:03}", self.session, self.number)
    }
}

impl FromStr for TerminalName {
    type Err = InvalidName;

    /// Split at the last `-`: the session is everything before it.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid = || InvalidName::Terminal(text.to_owned());
        let (session, digits) = text.rsplit_once('-').ok_or_else(invalid)?;
        let session = SessionName::new(session).map_err(|_| invalid())?;
        let number = parse_canonical_number(digits).ok_or_else(invalid)?;
        Ok(Self { session, number })
    }
}

impl TryFrom<String> for TerminalName {
    type Error = InvalidName;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<TerminalName> for String {
    fn from(name: TerminalName) -> Self {
        name.to_string()
    }
}

#[cfg(test)]
pub mod strategies {
    //! proptest strategies for the validated types.
    use super::{Host, RemoteHost, SessionName, TerminalName};
    use proptest::prelude::*;

    pub fn session_name() -> impl Strategy<Value = SessionName> {
        "[a-zA-Z0-9_][a-zA-Z0-9_-]{0,15}".prop_map(|s| SessionName::new(s).unwrap())
    }

    pub fn host() -> impl Strategy<Value = Host> {
        let remote = "([a-z_][a-z0-9_-]{0,7}@)?[a-z0-9][a-z0-9.-]{0,15}"
            .prop_filter("'local' is reserved", |s| s != "local")
            .prop_map(|s| Host::Ssh(RemoteHost::new(s).unwrap()));
        prop_oneof![Just(Host::Local), remote]
    }

    pub fn terminal_name() -> impl Strategy<Value = TerminalName> {
        (session_name(), any::<u32>()).prop_map(|(s, n)| TerminalName::new(&s, n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn session(name: &str) -> SessionName {
        SessionName::new(name).unwrap()
    }

    /// PLAN-terminal-here decision 2: the command splits as a shell splits
    /// it, the placeholders become the names in every word they appear in,
    /// other braces stay, and the text round-trips through serde.
    #[test]
    fn a_window_command_substitutes_the_session_and_the_shell() {
        let template =
            WindowTemplate::new(r#"my-window --name "{session} / {shell}" {shell} '{x}'"#).unwrap();
        let shell = TerminalName::new(&session("ws4"), 2);
        assert_eq!(
            template.argv(&shell),
            ["my-window", "--name", "ws4 / ws4-002", "ws4-002", "{x}"]
        );
        let json = serde_json::to_string(&template).unwrap();
        assert_eq!(
            serde_json::from_str::<WindowTemplate>(&json).unwrap(),
            template
        );
        for bad in ["", "   ", "sh -c 'open"] {
            assert!(
                matches!(
                    WindowTemplate::new(bad),
                    Err(InvalidName::WindowCommand { .. })
                ),
                "{bad:?}"
            );
            assert!(serde_json::from_str::<WindowTemplate>(&format!("{bad:?}")).is_err());
        }
    }

    #[test]
    fn valid_session_names() {
        for name in ["my-session", "session_123", "abc123", "ws4"] {
            assert!(SessionName::new(name).is_ok(), "{name}");
        }
    }

    #[test]
    fn invalid_session_names() {
        for name in [
            "",
            "my session",
            "my/session",
            "my;session",
            "-f",
            "a:b",
            "a.b",
        ] {
            assert!(
                matches!(SessionName::new(name), Err(InvalidName::Session { .. })),
                "{name}"
            );
        }
    }

    #[test]
    fn default_session_for_workspace_is_valid() {
        let name = SessionName::for_workspace(4);
        assert_eq!(name.as_str(), "ws4");
        assert_eq!(SessionName::new("ws4").unwrap(), name);
    }

    #[test]
    fn valid_remote_hosts() {
        for host in ["myserver", "server.example.com", "user@server", "localhost"] {
            assert!(RemoteHost::new(host).is_ok(), "{host}");
        }
    }

    #[test]
    fn invalid_remote_hosts() {
        for host in [
            "",
            "@server",
            "user@",
            "user name@server",
            "-oProxyCommand=x",
            "local",
            "host:22",
            "a@b@c",
        ] {
            assert!(
                matches!(RemoteHost::new(host), Err(InvalidName::Host { .. })),
                "{host}"
            );
        }
    }

    #[test]
    fn host_from_remote_arg() {
        assert_eq!(Host::from_remote_arg(None).unwrap(), Host::Local);
        assert_eq!(
            Host::from_remote_arg(Some("deepthought".into())).unwrap(),
            Host::Ssh(RemoteHost::new("deepthought").unwrap())
        );
        assert!(Host::from_remote_arg(Some("-x".into())).is_err());
    }

    #[test]
    fn host_serializes_as_its_label() {
        let json = serde_json::to_string(&Host::Local).unwrap();
        assert_eq!(json, "\"local\"");
        let remote: Host = serde_json::from_str("\"u@h.example\"").unwrap();
        assert_eq!(remote.label(), "u@h.example");
        assert!(serde_json::from_str::<Host>("\"-bad\"").is_err());
    }

    #[test]
    fn terminal_name_display_pads_to_three_digits() {
        assert_eq!(TerminalName::new(&session("ws4"), 2).to_string(), "ws4-002");
        assert_eq!(
            TerminalName::new(&session("ws4"), 1234).to_string(),
            "ws4-1234"
        );
    }

    #[test]
    fn terminal_name_prefix_is_exact() {
        let ws4 = session("ws4");
        assert_eq!(
            TerminalName::parse(&ws4, "ws4-001"),
            Some(TerminalName::new(&ws4, 1))
        );
        // Another session whose name starts with "ws4-" never matches.
        assert_eq!(TerminalName::parse(&ws4, "ws4-x-001"), None);
        assert_eq!(TerminalName::parse(&ws4, "ws4-001-002"), None);
        // Nor a session that merely shares a prefix.
        assert_eq!(TerminalName::parse(&ws4, "ws40-001"), None);
        assert_eq!(TerminalName::parse(&session("ws"), "ws4-001"), None);
        // The number is canonical digits only.
        for bad in ["ws4-", "ws4-01", "ws4-0001", "ws4-+01", "ws4-1a", "ws4001"] {
            assert_eq!(TerminalName::parse(&ws4, bad), None, "{bad}");
        }
        // The session of a longer name is parsed as the longer session.
        let other: TerminalName = "ws4-x-001".parse().unwrap();
        assert_eq!(other.session().as_str(), "ws4-x");
    }

    #[test]
    fn terminal_name_from_str_rejects_malformed() {
        for bad in [
            "",
            "ws4",
            "-001",
            "ws4-",
            "ws 4-001",
            "ws4-0001",
            "ws4-99999999999",
        ] {
            assert!(bad.parse::<TerminalName>().is_err(), "{bad}");
        }
    }

    proptest! {
        #[test]
        fn terminal_name_round_trips(name in strategies::terminal_name()) {
            let text = name.to_string();
            prop_assert_eq!(text.parse::<TerminalName>().unwrap(), name.clone());
            prop_assert_eq!(TerminalName::parse(name.session(), &text), Some(name.clone()));
            let json = serde_json::to_string(&name).unwrap();
            prop_assert_eq!(serde_json::from_str::<TerminalName>(&json).unwrap(), name);
        }

        #[test]
        fn host_label_round_trips(host in strategies::host()) {
            prop_assert_eq!(Host::from_label(host.label()).unwrap(), host);
        }

        #[test]
        fn session_name_never_matches_foreign_terminal(
            a in strategies::session_name(),
            b in strategies::session_name(),
            n in any::<u32>(),
        ) {
            prop_assume!(a != b);
            let foreign = TerminalName::new(&b, n).to_string();
            prop_assert_eq!(TerminalName::parse(&a, &foreign), None);
        }
    }
}
