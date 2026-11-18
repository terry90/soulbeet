use shared::failure::{Failure, FailureCode, RetryAdvice, Service, Stage};
use thiserror::Error;

/// What went wrong asking a metadata provider. `service` is the provider,
/// `Musicbrainz` or `Lastfm`; `status` is the status it gave the error, an
/// HTTP status or, for Last.fm's error body, Last.fm's error code, which
/// chooses the variant; `raw` is what it answered, or why it did not.
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
    #[error("{} refused Soulbeet's API key: {raw}", name(*.service))]
    NotConfigured {
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
        Service::Lastfm => "Last.fm",
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
            MetadataError::NotConfigured {
                service,
                status,
                raw,
            } => (ProviderNotConfigured, NeedsUser, service, status, raw),
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
        let failure = Failure::new(code, stage, retry, message).with_cause(
            *service,
            status.map(i32::from),
            raw.clone(),
        );
        match self {
            MetadataError::NotConfigured {
                service: Service::Lastfm,
                ..
            } => failure.with_user_action(crate::lastfm::configure_lastfm()),
            _ => failure,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::failure::UserActionCode;

    #[test]
    fn metadata_errors_map_to_failure_codes() {
        use FailureCode::*;
        use RetryAdvice::*;
        let mb = Service::Musicbrainz;
        for (e, code, retry) in [
            (MetadataError::NotFound { service: mb, status: None, raw: "Musicbrainz returned \"Not found\" for query \"https://musicbrainz.org/ws/2/release/8c1e?inc=recordings\"".into() }, MetadataNotFound, OtherQuery),
            (MetadataError::RateLimited { service: mb, status: None, raw: "max retries".into() }, MetadataRateLimited, SameSourceLater),
            (MetadataError::Unavailable { service: mb, status: Some(502), raw: "502".into() }, MetadataUnavailable, SameSourceLater),
            (MetadataError::NotConfigured { service: Service::Lastfm, status: Some(10), raw: "Invalid API key - You must be granted a valid key by last.fm".into() }, ProviderNotConfigured, NeedsUser),
            (MetadataError::Rejected { service: mb, status: None, raw: "Invalid mbid.".into() }, InvalidInput, NotRetryable),
            (MetadataError::Unreadable { service: mb, status: None, raw: "missing field `id`".into() }, BackendResponseUnreadable, NotRetryable),
        ] {
            let f = e.failure(Stage::Metadata, e.to_string());
            assert_eq!((f.code, f.retry, f.stage), (code, retry, Stage::Metadata), "{e:?}");
        }
        let key = MetadataError::NotConfigured {
            service: Service::Lastfm,
            status: Some(10),
            raw: "x".into(),
        }
        .failure(Stage::Metadata, "m");
        assert_eq!(
            key.user_action.unwrap().code,
            UserActionCode::ConfigureProvider
        );
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
