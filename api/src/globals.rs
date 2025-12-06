#[cfg(feature = "server")]
use std::collections::HashMap;
#[cfg(feature = "server")]
use std::sync::LazyLock;

#[cfg(feature = "server")]
use shared::slskd::FileEntry;
#[cfg(feature = "server")]
use soulbeet::slskd::{SoulseekClient, SoulseekClientBuilder};
#[cfg(feature = "server")]
use tokio::sync::{broadcast, RwLock};

#[cfg(feature = "server")]
pub static SLSKD_CLIENT: LazyLock<SoulseekClient> = LazyLock::new(|| {
    let api_key = std::env::var("SLSKD_API_KEY").expect("Missing SLSKD_API_KEY env var");
    let base_url = std::env::var("SLSKD_URL").expect("Missing SLSKD_URL env var");
    let download_path =
        std::env::var("SLSKD_DOWNLOAD_PATH").expect("Missing SLSKD_DOWNLOAD_PATH env var");

    SoulseekClientBuilder::new()
        .api_key(&api_key)
        .base_url(&base_url)
        .download_path(&download_path)
        .build()
        .expect("Failed to create Soulseek client")
});

#[cfg(feature = "server")]
pub static USER_CHANNELS: LazyLock<RwLock<HashMap<String, broadcast::Sender<Vec<FileEntry>>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
