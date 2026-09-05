use std::fmt;
use std::io;

#[derive(Debug)]
pub enum Error {
    Message(String),
    Http {
        code: u16,
        url: String,
        hint: String,
        body: Vec<u8>,
    },
    /// URL looks like HLS but the response is a progressive media file.
    Progressive {
        url: String,
    },
}

impl Error {
    pub fn msg(m: impl Into<String>) -> Self {
        Self::Message(m.into())
    }

    pub fn exit_code(&self) -> i32 {
        1
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Message(s) => write!(f, "{s}"),
            Error::Http {
                code, url, hint, ..
            } => write!(f, "play: HTTP {code} fetching {url}{hint}"),
            Error::Progressive { url } => {
                write!(f, "play: {url} is a media file, not an HLS playlist")
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::msg(format!("play: {e}"))
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::msg(format!("play: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, Error>;
