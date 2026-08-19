use thiserror::Error;

/// Engine-wide error type. Transport- and protocol-specific variants get added
/// as the corresponding modules are fleshed out in later phases.
#[derive(Debug, Error)]
pub enum Error {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),

    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("real-debrid error (status {status}, code {code:?}): {message}")]
    RealDebrid {
        status: u16,
        code: Option<i64>,
        message: String,
    },

    #[error("aria2 rpc error ({method}, code {code}): {message}")]
    Aria2 {
        method: String,
        code: i64,
        message: String,
    },

    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("incomplete download: got {got} of {expected} bytes")]
    Incomplete { got: i64, expected: i64 },

    #[error("cancelled")]
    Cancelled,

    #[error("invalid mega link: {0}")]
    InvalidLink(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
