use std::fmt;

#[derive(Debug)]
pub enum Error {
    Sqlite(String),
    Io { path: String, message: String },
    Config(String),
    InvalidPath(String),
    Blocked(String),
    Unsupported(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Sqlite(m) => write!(f, "database error: {m}"),
            Error::Io { path, message } => {
                if path.is_empty() {
                    write!(f, "filesystem error: {message}")
                } else {
                    write!(f, "filesystem error at {path}: {message}")
                }
            }
            Error::Config(m) => write!(f, "configuration error: {m}"),
            Error::InvalidPath(m) => write!(f, "path is not a usable absolute path: {m}"),
            Error::Blocked(m) => write!(f, "operation blocked: {m}"),
            Error::Unsupported(m) => write!(f, "unsupported: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Sqlite(e.to_string())
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io {
            path: String::new(),
            message: e.to_string(),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn io_at(path: &str, e: std::io::Error) -> Error {
    Error::Io {
        path: path.to_string(),
        message: e.to_string(),
    }
}
