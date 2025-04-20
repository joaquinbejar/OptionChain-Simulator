use std::fmt::Display;

/// Represents address binding options for the server
#[derive(Debug, Clone, Copy, Default)]
pub enum ListenOn {
    /// Binds to all network interfaces (0.0.0.0)
    All,
    /// Binds only to localhost (127.0.0.1)
    #[default]
    Localhost,
}

impl ListenOn {
    /// Converts the enum variant to its corresponding IP address string
    pub fn as_str(&self) -> &'static str {
        match self {
            ListenOn::All => "0.0.0.0",
            ListenOn::Localhost => "127.0.0.1",
        }
    }
}

impl From<ListenOn> for String {
    fn from(listen_on: ListenOn) -> Self {
        listen_on.as_str().to_string()
    }
}

impl Display for ListenOn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}
