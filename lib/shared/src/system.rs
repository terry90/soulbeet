use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SystemHealth {
    pub slskd_online: bool,
    pub beets_ready: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BackendInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AvailableBackends {
    pub metadata: Vec<BackendInfo>,
    pub download: Vec<BackendInfo>,
    pub importer: Vec<BackendInfo>,
}
