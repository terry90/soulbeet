use std::collections::HashMap;
use std::sync::Arc;

use crate::{DownloadBackend, MetadataProvider, MusicImporter};

pub struct Services {
    metadata: HashMap<String, Arc<dyn MetadataProvider>>,
    download: HashMap<String, Arc<dyn DownloadBackend>>,
    importer: HashMap<String, Arc<dyn MusicImporter>>,
    default_metadata: Option<String>,
    default_download: Option<String>,
    default_importer: Option<String>,
}

impl Services {
    pub fn metadata(&self, id: Option<&str>) -> Option<&Arc<dyn MetadataProvider>> {
        let key = id.or(self.default_metadata.as_deref())?;
        self.metadata.get(key)
    }

    pub fn download(&self, id: Option<&str>) -> Option<&Arc<dyn DownloadBackend>> {
        let key = id.or(self.default_download.as_deref())?;
        self.download.get(key)
    }

    pub fn importer(&self, id: Option<&str>) -> Option<&Arc<dyn MusicImporter>> {
        let key = id.or(self.default_importer.as_deref())?;
        self.importer.get(key)
    }

    pub fn list_metadata(&self) -> Vec<(&str, &str)> {
        self.metadata
            .values()
            .map(|p| (p.id(), p.name()))
            .collect()
    }

    pub fn list_downloads(&self) -> Vec<(&str, &str)> {
        self.download
            .values()
            .map(|p| (p.id(), p.name()))
            .collect()
    }

    pub fn list_importers(&self) -> Vec<(&str, &str)> {
        self.importer
            .values()
            .map(|p| (p.id(), p.name()))
            .collect()
    }
}

pub struct ServicesBuilder {
    metadata: HashMap<String, Arc<dyn MetadataProvider>>,
    download: HashMap<String, Arc<dyn DownloadBackend>>,
    importer: HashMap<String, Arc<dyn MusicImporter>>,
    default_metadata: Option<String>,
    default_download: Option<String>,
    default_importer: Option<String>,
}

impl ServicesBuilder {
    pub fn new() -> Self {
        Self {
            metadata: HashMap::new(),
            download: HashMap::new(),
            importer: HashMap::new(),
            default_metadata: None,
            default_download: None,
            default_importer: None,
        }
    }

    pub fn add_metadata(mut self, provider: impl MetadataProvider + 'static) -> Self {
        let id = provider.id().to_string();
        if self.default_metadata.is_none() {
            self.default_metadata = Some(id.clone());
        }
        self.metadata.insert(id, Arc::new(provider));
        self
    }

    pub fn add_download(mut self, backend: impl DownloadBackend + 'static) -> Self {
        let id = backend.id().to_string();
        if self.default_download.is_none() {
            self.default_download = Some(id.clone());
        }
        self.download.insert(id, Arc::new(backend));
        self
    }

    pub fn add_importer(mut self, importer: impl MusicImporter + 'static) -> Self {
        let id = importer.id().to_string();
        if self.default_importer.is_none() {
            self.default_importer = Some(id.clone());
        }
        self.importer.insert(id, Arc::new(importer));
        self
    }

    pub fn default_metadata(mut self, id: &str) -> Self {
        self.default_metadata = Some(id.to_string());
        self
    }

    pub fn default_download(mut self, id: &str) -> Self {
        self.default_download = Some(id.to_string());
        self
    }

    pub fn default_importer(mut self, id: &str) -> Self {
        self.default_importer = Some(id.to_string());
        self
    }

    pub fn build(self) -> Result<Services, &'static str> {
        if self.metadata.is_empty() {
            return Err("at least one metadata provider required");
        }
        if self.download.is_empty() {
            return Err("at least one download backend required");
        }
        if self.importer.is_empty() {
            return Err("at least one music importer required");
        }

        Ok(Services {
            metadata: self.metadata,
            download: self.download,
            importer: self.importer,
            default_metadata: self.default_metadata,
            default_download: self.default_download,
            default_importer: self.default_importer,
        })
    }
}

impl Default for ServicesBuilder {
    fn default() -> Self {
        Self::new()
    }
}
