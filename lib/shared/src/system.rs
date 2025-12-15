use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SystemHealth {
    pub slskd_online: bool,
    pub beets_ready: bool,
}
