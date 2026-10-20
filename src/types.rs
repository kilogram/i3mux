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
//! - [`Token`]: always 32 lowercase hex digits, so never empty and safe as a
//!   shell argument.
//! - [`SwayInstance`]: equal only for the same Sway run in the same boot.

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

/// The token of one take of a session (plan-layout-backup decision 1): 16
/// random bytes, written as 32 lowercase hex digits. Tokens are compared for
/// equality only; they carry no order and no time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Token(String);

/// Text that is not a [`Token`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid session token '{0}': expected 32 lowercase hex digits")]
pub struct InvalidToken(String);

impl Token {
    const HEX_DIGITS: usize = 32;

    /// Validate `text` as a token.
    ///
    /// # Errors
    /// [`InvalidToken`] unless `text` is exactly 32 digits `0-9a-f`.
    pub fn parse(text: &str) -> Result<Self, InvalidToken> {
        let is_hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
        if text.len() == Self::HEX_DIGITS && text.bytes().all(is_hex) {
            Ok(Self(text.to_owned()))
        } else {
            Err(InvalidToken(text.to_owned()))
        }
    }

    /// The token as its hex digits.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Token {
    type Error = InvalidToken;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<Token> for String {
    fn from(token: Token) -> Self {
        token.0
    }
}

/// One run of Sway: the id of the boot it runs in and its IPC socket path.
/// A reboot or a restarted Sway is another instance. The owner of a binding
/// is the instance that holds its windows (plan-layout-backup decision 2).
/// Compared for equality only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwayInstance {
    boot_id: String,
    socket: String,
}

impl SwayInstance {
    /// The Sway that listens on `socket` in the boot `boot_id`.
    #[must_use]
    pub fn new(boot_id: impl Into<String>, socket: impl Into<String>) -> Self {
        Self {
            boot_id: boot_id.into(),
            socket: socket.into(),
        }
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

    #[test]
    fn token_is_exactly_32_lowercase_hex_digits() {
        let valid = "0123456789abcdef0123456789abcdef";
        assert_eq!(Token::parse(valid).unwrap().as_str(), valid);
        for bad in [
            "",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
            "0123456789ABCDEF0123456789abcdef",
            "0123456789abcdef0123456789abcdeg",
            " 123456789abcdef0123456789abcdef",
        ] {
            assert!(Token::parse(bad).is_err(), "{bad:?}");
        }
        assert!(serde_json::from_str::<Token>("\"\"").is_err());
    }

    proptest! {
        #[test]
        fn token_of_any_16_bytes_round_trips(bytes in any::<[u8; 16]>()) {
            let hex = bytes.iter().fold(String::new(), |mut hex, b| {
                use std::fmt::Write as _;
                write!(hex, "{b:02x}").unwrap();
                hex
            });
            let token = Token::parse(&hex).unwrap();
            let json = serde_json::to_string(&token).unwrap();
            prop_assert_eq!(json, format!("\"{hex}\""));
            prop_assert_eq!(serde_json::from_str::<Token>(&format!("\"{hex}\"")).unwrap(), token);
        }

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
