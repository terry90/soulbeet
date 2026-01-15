#[cfg(feature = "server")]
use std::collections::HashMap;
#[cfg(feature = "server")]
use std::sync::LazyLock;

#[cfg(feature = "server")]
use shared::slskd::FileEntry;
#[cfg(feature = "server")]
use soulbeet::slskd::{SoulseekClient, DownloadConfig, SoulseekClientBuilder};
#[cfg(feature = "server")]
use tokio::sync::{broadcast, RwLock};

#[cfg(feature = "server")]
pub static SLSKD_CLIENT: LazyLock<SoulseekClient> = LazyLock::new(|| {
    let api_key = std::env::var("SLSKD_API_KEY").expect("Missing SLSKD_API_KEY env var");
    let base_url = std::env::var("SLSKD_URL").expect("Missing SLSKD_URL env var");

    SoulseekClientBuilder::new()
        .api_key(&api_key)
        .base_url(&base_url)
        .download_config(DownloadConfig {
            batch_size: 3,
            batch_delay_ms: 3000,
            max_retries: 3,
            retry_base_delay_ms: 1000,
        })
        .build()
        .expect("Failed to create Soulseek client")
});

#[cfg(feature = "server")]
pub static USER_CHANNELS: LazyLock<RwLock<HashMap<String, broadcast::Sender<Vec<FileEntry>>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
