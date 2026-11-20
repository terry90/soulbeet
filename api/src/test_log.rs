//! The messages api tests log, for tests that check what reaches the log.

use std::cell::RefCell;
use std::fmt::{Debug, Write};
use std::sync::Once;

use tracing::subscriber::Interest;

/// The messages logged on this thread while it lives.
///
/// A global subscriber, installed once, keeps them per thread: a scoped one
/// would race the interest other test threads cache for the same log lines.
pub(crate) struct LogCapture(());

thread_local! {
    static CAPTURED: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

impl LogCapture {
    pub(crate) fn start() -> Self {
        static INSTALLED: Once = Once::new();
        INSTALLED.call_once(|| {
            tracing::subscriber::set_global_default(Collector)
                .expect("no other test sets a global subscriber");
        });
        // A line first logged on another thread while it was installed may
        // have cached that nobody listens.
        tracing::callsite::rebuild_interest_cache();
        CAPTURED.with(|lines| *lines.borrow_mut() = Some(Vec::new()));
        LogCapture(())
    }

    pub(crate) fn lines(&self) -> Vec<String> {
        CAPTURED.with(|lines| lines.borrow().clone().unwrap_or_default())
    }
}

impl Drop for LogCapture {
    fn drop(&mut self) {
        CAPTURED.with(|lines| *lines.borrow_mut() = None);
    }
}

struct Collector;

impl tracing::Subscriber for Collector {
    /// Asked at each event: whether its thread captures changes.
    fn register_callsite(&self, _: &'static tracing::Metadata<'static>) -> Interest {
        Interest::sometimes()
    }
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        CAPTURED.with(|lines| lines.borrow().is_some())
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Message<'a>(&'a mut String);
        impl tracing::field::Visit for Message<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn Debug) {
                if field.name() == "message" {
                    // The message's Debug is its text as written.
                    let _ = write!(self.0, "{value:?}");
                }
            }
        }
        let mut line = String::new();
        event.record(&mut Message(&mut line));
        CAPTURED.with(|lines| {
            if let Some(lines) = lines.borrow_mut().as_mut() {
                lines.push(line);
            }
        });
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}
