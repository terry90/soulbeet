use shared::failure::{Failure, FailureCode, Stage};
use std::fmt;
use std::sync::Arc;

/// What a download backend's own error says to Soulful. Each backend keeps
/// its own error type and words its failures itself.
pub trait BackendFault: fmt::Display + fmt::Debug + Send + Sync {
    /// This error as the failure of `stage`, shown as `message`.
    fn failure(&self, stage: Stage, message: String) -> Failure;
    /// This error of `ask` as a failure at `stage`, with the backend's own
    /// message for the ask.
    fn ask_failure(&self, ask: PeerAsk, stage: Stage) -> Failure;
    /// Whether the backend answered with something Soulful cannot read, which
    /// will not read later either.
    fn answer_unreadable(&self) -> bool;
}

/// The error of every `DownloadBackend` call.
#[derive(Clone)]
pub struct BackendError(Arc<dyn BackendFault>);

pub type BackendResult<T> = Result<T, BackendError>;

impl<E: BackendFault + 'static> From<E> for BackendError {
    fn from(e: E) -> Self {
        BackendError(Arc::new(e))
    }
}

impl BackendError {
    pub fn failure(&self, stage: Stage, message: impl Into<String>) -> Failure {
        self.0.failure(stage, message.into())
    }

    pub fn ask_failure(&self, ask: PeerAsk, stage: Stage) -> Failure {
        self.0.ask_failure(ask, stage)
    }

    pub fn answer_unreadable(&self) -> bool {
        self.0.answer_unreadable()
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Debug for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

/// A question Soulful asks a backend about a source outside the calls of a
/// transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerAsk {
    /// The transfer's place in the source's queue, asked of the source now.
    QueuePosition,
    /// Whether the source is online now.
    Presence,
}

impl PeerAsk {
    /// Who did not answer when the ask gets no answer in time: the source for
    /// a place in its queue, the backend's network for a presence.
    pub fn unanswered(self) -> FailureCode {
        match self {
            PeerAsk::QueuePosition => FailureCode::TransferConnectionFailed,
            PeerAsk::Presence => FailureCode::BackendUnreachable,
        }
    }
}
