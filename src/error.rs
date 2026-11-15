use std::future::Future;
use std::sync::{Arc, Mutex};

use dav_server::fs::FsError;
use thiserror::Error;

pub type AppResult<T> = std::result::Result<T, AppError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DavHttpFault {
    TokenUnavailable,
    RateLimited,
}

tokio::task_local! {
    static DAV_HTTP_FAULT: Arc<Mutex<Option<DavHttpFault>>>;
}

/// Runs `fut` inside a request-scoped fault tracker so WebDAV handlers can translate
/// `TokenUnavailable` -> `401` and `RateLimited` -> `503 + Retry-After`.
pub async fn scope_dav_fault<F, R>(fut: F) -> (R, Option<DavHttpFault>)
where
    F: Future<Output = R>,
{
    let slot = Arc::new(Mutex::new(None));
    let res = DAV_HTTP_FAULT.scope(Arc::clone(&slot), fut).await;
    let fault = *slot.lock().expect("dav fault lock poisoned");
    (res, fault)
}

fn record_dav_fault(fault: DavHttpFault) {
    let _ = DAV_HTTP_FAULT.try_with(|slot| {
        if let Ok(mut guard) = slot.lock() {
            *guard = Some(fault);
        }
    });
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("OAuth authorization required: {0}")]
    TokenUnavailable(String),

    #[error("OAuth error ({error}): {description}")]
    OAuth { error: String, description: String },

    #[error("Feishu API error (code={code}, http={http_status}): {msg}")]
    FeishuApi {
        code: i64,
        msg: String,
        http_status: u16,
    },

    #[error("Feishu API rate limited")]
    RateLimited,

    #[error("resource not found: {0}")]
    NotFound(String),

    #[error("resource already exists: {0}")]
    AlreadyExists(String),

    #[error("operation forbidden: {0}")]
    Forbidden(String),

    #[error("operation not implemented: {0}")]
    NotImplemented(String),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("HTTP client error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

impl AppError {
    /// Returns true if the error indicates an expired or invalid access token.
    pub fn is_token_invalid(&self) -> bool {
        match self {
            AppError::FeishuApi {
                code, http_status, ..
            } => {
                *http_status == 401
                    || matches!(
                        *code,
                        99991661 | 99991663 | 99991664 | 99991668 | 99991677 | 20005
                    )
            }
            _ => false,
        }
    }

    /// Returns true if the error indicates Feishu Drive QPS rate limiting.
    pub fn is_rate_limited(&self) -> bool {
        match self {
            AppError::RateLimited => true,
            AppError::FeishuApi {
                code, http_status, ..
            } => *http_status == 429 || *code == 1061045,
            _ => false,
        }
    }

    /// Maps known Feishu API error codes to richer semantic errors when possible.
    pub fn from_feishu_code(code: i64, msg: String, http_status: u16) -> Self {
        match code {
            1061045 => AppError::RateLimited,
            1062502 | 1061007 => AppError::NotFound(msg),
            1061004 => AppError::Forbidden(msg),
            _ if http_status == 429 => AppError::RateLimited,
            _ if http_status == 404 => AppError::NotFound(msg),
            _ if http_status == 403 => AppError::Forbidden(msg),
            _ => AppError::FeishuApi {
                code,
                msg,
                http_status,
            },
        }
    }
}

impl From<&AppError> for FsError {
    fn from(err: &AppError) -> Self {
        match err {
            AppError::NotFound(_) => FsError::NotFound,
            AppError::AlreadyExists(_) => FsError::Exists,
            AppError::TokenUnavailable(_) => {
                record_dav_fault(DavHttpFault::TokenUnavailable);
                FsError::Forbidden
            }
            AppError::Forbidden(_) => FsError::Forbidden,
            AppError::NotImplemented(_) => FsError::NotImplemented,
            AppError::InvalidInput(_) => FsError::Forbidden,
            AppError::RateLimited => {
                record_dav_fault(DavHttpFault::RateLimited);
                FsError::IsRemote
            }
            AppError::FeishuApi { http_status, .. } => {
                if err.is_rate_limited() {
                    record_dav_fault(DavHttpFault::RateLimited);
                    return FsError::IsRemote;
                }
                match *http_status {
                    404 => FsError::NotFound,
                    401 => {
                        record_dav_fault(DavHttpFault::TokenUnavailable);
                        FsError::Forbidden
                    }
                    403 => FsError::Forbidden,
                    409 | 412 => FsError::Exists,
                    413 => FsError::TooLarge,
                    507 => FsError::InsufficientStorage,
                    500..=599 => FsError::IsRemote,
                    _ => FsError::GeneralFailure,
                }
            }
            AppError::Http(_) => FsError::IsRemote,
            AppError::Config(_) | AppError::OAuth { .. } | AppError::Io(_) | AppError::Json(_) => {
                FsError::GeneralFailure
            }
        }
    }
}

impl From<AppError> for FsError {
    fn from(err: AppError) -> Self {
        (&err).into()
    }
}
