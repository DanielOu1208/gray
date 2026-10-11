use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The structured fields a provider failure carries alongside its class.
/// Populated where the raw response exists (`classify_http_error`); every
/// downstream consumer reads fields here instead of re-deriving them from
/// formatted message text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDetail {
    /// HTTP status of the failed response, when one came back.
    pub status: Option<u16>,
    /// The provider's own error code/type label (OpenAI `error.code`,
    /// `error.type`), when the body carried one.
    pub vendor_code: Option<String>,
    /// The provider's own message when the body carried one — the
    /// preferred human-readable detail, no envelope scraping.
    pub vendor_message: Option<String>,
    /// `cf-ray` edge correlation id, when present.
    pub cf_ray: Option<String>,
    /// The provider's request id, when present.
    pub request_id: Option<String>,
}

/// Errors that can occur within the core agent harness.
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Provider error: {0}")]
    Provider(String),

    // Provider failures keep their taxonomy (mirrors `ProviderError`) so
    // callers can branch on the class instead of parsing the message.
    #[error("auth failed: {0}")]
    Auth(String),

    #[error("rate limited: {0}")]
    RateLimited(String),

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("context exhausted — start /new or compact: {0}")]
    ContextOverflow(String),

    #[error("server error: {0}")]
    ServerError(String),

    #[error("stream broken: {0}")]
    Stream(String),

    #[error("Connection failed: {0}")]
    Connection(String),

    #[error("Request timed out: {0}")]
    Timeout(String),

    #[error("Tool loop detected: {0}")]
    LoopDetected(String),

    #[error("Operation cancelled")]
    Cancelled,

    /// A provider failure carrying the response's structured fields:
    /// `class` is the same taxonomy as the flat variants (the message text
    /// is unchanged), `detail` is what the classifier parsed out of the
    /// response instead of only formatting into the message. Everything
    /// that branches on the class must see through the wrap —
    /// [`CoreError::class`], [`CoreError::code`], [`CoreError::retryable`]
    /// already do.
    #[error("{class}")]
    Detailed {
        class: Box<CoreError>,
        detail: ProviderDetail,
    },
}

impl CoreError {
    /// Stable machine-readable class. Same string the `--json` error record
    /// carries, so harnesses and humans read one vocabulary.
    /// The innermost failure class: `Detailed` wraps a plain variant, so
    /// callers matching on the taxonomy never have to unwrap by hand.
    pub fn class(&self) -> &CoreError {
        let mut cur = self;
        while let Self::Detailed { class, .. } = cur {
            cur = class;
        }
        cur
    }

    /// Structured fields when this failure carries them — `Detailed`
    /// errors only; `None` for every plain variant.
    pub fn detail(&self) -> Option<&ProviderDetail> {
        if let Self::Detailed { detail, .. } = self {
            Some(detail)
        } else {
            None
        }
    }

    /// Stable machine-readable class. Same string the `--json` error record
    /// carries, so harnesses and humans read one vocabulary.
    pub fn code(&self) -> &'static str {
        match self.class() {
            Self::Auth(_) => "auth_failed",
            Self::RateLimited(_) => "rate_limited",
            Self::BadRequest(_) => "bad_request",
            Self::ContextOverflow(_) => "context_overflow",
            Self::ServerError(_) => "server_error",
            Self::Stream(_) => "stream_broken",
            Self::Connection(_) => "connection_failed",
            Self::Timeout(_) => "timeout",
            Self::LoopDetected(_) => "loop_detected",
            Self::Cancelled => "cancelled",
            Self::Serialization(_) => "serialization",
            Self::Provider(_) => "provider_error",
            // Unreachable through `class()` — kept for exhaustiveness.
            Self::Detailed { class, .. } => class.code(),
        }
    }

    /// True when re-running the same turn can plausibly succeed: the provider
    /// or network failed, not the request. Auth/bad-request/context failures
    /// repeat identically until the configuration or session changes.
    pub fn retryable(&self) -> bool {
        matches!(
            self.class(),
            Self::RateLimited(_)
                | Self::ServerError(_)
                | Self::Stream(_)
                | Self::Connection(_)
                | Self::Timeout(_)
        )
    }
}

/// Convenience type alias for Result with CoreError.
pub type Result<T> = std::result::Result<T, CoreError>;
