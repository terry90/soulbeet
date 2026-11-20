use shared::failure::{Failure, FailureCode, RetryAdvice, Service, Stage};
use thiserror::Error;

/// What went wrong asking a metadata provider. `service` is the provider;
/// `status` is the HTTP status it gave; `raw` is what it answered, or why it
/// did not.
#[derive(Debug, Clone, Error)]
pub enum MetadataError {
    #[error("{} found nothing: {raw}", name(*.service))]
    NotFound {
        service: Service,
        status: Option<u16>,
        raw: String,
    },
    #[error("{} is rate limiting Soulbeet: {raw}", name(*.service))]
    RateLimited {
        service: Service,
        status: Option<u16>,
        raw: String,
    },
    #[error("{} is unavailable: {raw}", name(*.service))]
    Unavailable {
        service: Service,
        status: Option<u16>,
        raw: String,
    },
    #[error("{} refused the request: {raw}", name(*.service))]
    Rejected {
        service: Service,
        status: Option<u16>,
        raw: String,
    },
    #[error("Unreadable {} answer: {raw}", name(*.service))]
    Unreadable {
        service: Service,
        status: Option<u16>,
        raw: String,
    },
}

/// The provider as its errors name it.
fn name(service: Service) -> &'static str {
    match service {
        Service::Musicbrainz => "MusicBrainz",
        other => unreachable!("only metadata providers give a MetadataError, not {other:?}"),
    }
}

impl MetadataError {
    /// Worth another attempt within the same call: only `Unavailable`.
    pub fn is_transient(&self) -> bool {
        matches!(self, MetadataError::Unavailable { .. })
    }

    /// This error as the failure of `stage`, shown as `message`.
    pub fn failure(&self, stage: Stage, message: impl Into<String>) -> Failure {
        use FailureCode::*;
        use RetryAdvice::*;
        let (code, retry, service, status, raw) = match self {
            MetadataError::NotFound {
                service,
                status,
                raw,
            } => (MetadataNotFound, OtherQuery, service, status, raw),
            MetadataError::RateLimited {
                service,
                status,
                raw,
            } => (MetadataRateLimited, SameSourceLater, service, status, raw),
            MetadataError::Unavailable {
                service,
                status,
                raw,
            } => (MetadataUnavailable, SameSourceLater, service, status, raw),
            MetadataError::Rejected {
                service,
                status,
                raw,
            } => (InvalidInput, NotRetryable, service, status, raw),
            MetadataError::Unreadable {
                service,
                status,
                raw,
            } => (
                BackendResponseUnreadable,
                NotRetryable,
                service,
                status,
                raw,
            ),
        };
        Failure::new(code, stage, retry, message).with_cause(
            *service,
            status.map(i32::from),
            raw.clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_errors_map_to_failure_codes() {
        use FailureCode::*;
        use RetryAdvice::*;
        let mb = Service::Musicbrainz;
        for (e, code, retry) in [
            (MetadataError::NotFound { service: mb, status: None, raw: "Musicbrainz returned \"Not found\" for query \"https://musicbrainz.org/ws/2/release/8c1e?inc=recordings\"".into() }, MetadataNotFound, OtherQuery),
            (MetadataError::RateLimited { service: mb, status: None, raw: "max retries".into() }, MetadataRateLimited, SameSourceLater),
            (MetadataError::Unavailable { service: mb, status: Some(502), raw: "502".into() }, MetadataUnavailable, SameSourceLater),
            (MetadataError::Rejected { service: mb, status: None, raw: "Invalid mbid.".into() }, InvalidInput, NotRetryable),
            (MetadataError::Unreadable { service: mb, status: None, raw: "missing field `id`".into() }, BackendResponseUnreadable, NotRetryable),
        ] {
            let f = e.failure(Stage::Metadata, e.to_string());
            assert_eq!((f.code, f.retry, f.stage), (code, retry, Stage::Metadata), "{e:?}");
        }
        assert!(MetadataError::Unavailable {
            service: mb,
            status: None,
            raw: String::new()
        }
        .is_transient());
        assert!(!MetadataError::RateLimited {
            service: mb,
            status: None,
            raw: String::new()
        }
        .is_transient());
    }
}
