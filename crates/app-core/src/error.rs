use cluster_core::ClusterError;
use thiserror::Error;

/// The storage failure type is defined in `cluster-core` so that the runtime
/// can use it too; the application layer knows it under this name.
pub use cluster_core::persist::StoreError as RepoError;

pub type AppResult<T> = Result<T, AppError>;
pub type RepoResult<T> = Result<T, RepoError>;

/// Application-level failure. `app-web` maps this onto HTTP status codes; it
/// is never constructed from a raw SQLx or reqwest error directly.
#[derive(Debug, Error)]
pub enum AppError {
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    Validation(String),
    /// Deliberately the same error for "no such user" and "wrong password",
    /// so the response cannot be used to enumerate accounts.
    #[error("invalid username or password")]
    Unauthorized,
    #[error("not permitted")]
    Forbidden,
    #[error("{0}")]
    Conflict(String),
    #[error("upstream provider: {0}")]
    Integration(String),
    #[error(transparent)]
    Cluster(#[from] ClusterError),
    #[error(transparent)]
    Repo(#[from] RepoError),
    #[error("internal error: {0}")]
    Internal(String),
}

impl AppError {
    pub fn validation(msg: impl Into<String>) -> Self {
        AppError::Validation(msg.into())
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        AppError::Internal(msg.into())
    }

    /// The HTTP status this error should produce. Kept here so the mapping is
    /// defined once, next to the variants.
    pub fn status_code(&self) -> u16 {
        match self {
            AppError::NotFound => 404,
            AppError::Validation(_) => 400,
            AppError::Unauthorized => 401,
            AppError::Forbidden => 403,
            AppError::Conflict(_) => 409,
            AppError::Integration(_) => 502,
            AppError::Cluster(ClusterError::UnknownNode(_))
            | AppError::Cluster(ClusterError::UnknownJob(_))
            | AppError::Cluster(ClusterError::UnknownTask(_)) => 404,
            AppError::Cluster(ClusterError::Invalid(_)) => 400,
            AppError::Repo(RepoError::NotFound) => 404,
            AppError::Repo(RepoError::Conflict(_)) => 409,
            _ => 500,
        }
    }

    /// Whether the detail is safe to show a user, or should be logged only.
    pub fn is_public(&self) -> bool {
        self.status_code() < 500
    }
}
