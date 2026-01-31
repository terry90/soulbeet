//! Resilient WebSocket client with automatic reconnection.
//!
//! This module is only compiled when the `web` feature is enabled (browser environment).

use dioxus::logger::tracing::{info, warn};
use dioxus::prelude::*;
use gloo_timers::future::TimeoutFuture;
use serde::de::DeserializeOwned;
use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;

/// Configuration for WebSocket reconnection behavior.
#[derive(Clone)]
pub struct ReconnectConfig {
    /// Initial delay before first reconnection attempt (in milliseconds).
    pub base_delay_ms: u32,
    /// Maximum delay between reconnection attempts (in milliseconds).
    pub max_delay_ms: u32,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            base_delay_ms: 1_000,
            max_delay_ms: 30_000,
        }
    }
}

impl ReconnectConfig {
    fn calculate_delay(&self, retry_count: u32) -> u32 {
        let base_delay = self
            .base_delay_ms
            .saturating_mul(2u32.saturating_pow(retry_count.min(5)));
        let delay = base_delay.min(self.max_delay_ms);
        let jitter = (delay / 4).saturating_mul(retry_count % 4) / 4;
        delay.saturating_add(jitter)
    }
}

pub fn use_resilient_websocket<T, F, Fut, C>(connect: C, on_message: F)
where
    T: DeserializeOwned + 'static,
    F: FnMut(T) + 'static,
    C: Fn() -> Fut + 'static,
    Fut: Future<Output = Result<dioxus::fullstack::Websocket<(), T>, ServerFnError>> + 'static,
{
    use_resilient_websocket_with_config(connect, on_message, ReconnectConfig::default())
}

pub fn use_resilient_websocket_with_config<T, F, Fut, C>(
    connect: C,
    on_message: F,
    config: ReconnectConfig,
) where
    T: DeserializeOwned + 'static,
    F: FnMut(T) + 'static,
    C: Fn() -> Fut + 'static,
    Fut: Future<Output = Result<dioxus::fullstack::Websocket<(), T>, ServerFnError>> + 'static,
{
    // Store callbacks in Rc<RefCell> to allow sharing across async boundaries
    let connect = use_hook(|| Rc::new(connect));
    let on_message = use_hook(|| Rc::new(RefCell::new(on_message)));

    use_future(move || {
        let connect = Rc::clone(&connect);
        let on_message = Rc::clone(&on_message);
        let config = config.clone();

        async move {
            let mut retry_count: u32 = 0;

            loop {
                info!(
                    "Connecting to WebSocket (attempt {})",
                    retry_count.saturating_add(1)
                );

                match connect().await {
                    Ok(socket) => {
                        retry_count = 0;
                        receive_messages(socket, &on_message).await;
                    }
                    Err(e) => {
                        warn!("Failed to establish WebSocket connection: {:?}", e);
                    }
                }

                retry_count = retry_count.saturating_add(1);
                let delay = config.calculate_delay(retry_count);

                warn!(
                    "WebSocket disconnected, reconnecting in {}ms (attempt {})",
                    delay, retry_count
                );
                TimeoutFuture::new(delay).await;
            }
        }
    });
}

async fn receive_messages<T, F>(
    socket: dioxus::fullstack::Websocket<(), T>,
    on_message: &Rc<RefCell<F>>,
) where
    T: DeserializeOwned,
    F: FnMut(T),
{
    loop {
        match socket.recv().await {
            Ok(data) => on_message.borrow_mut()(data),
            Err(e) => {
                warn!("WebSocket receive error: {:?}", e);
                break;
            }
        }
    }
}
