use dioxus::prelude::*;
use shared::download::AutoDownloadEvent;

#[derive(Clone, Copy)]
pub struct SearchReset(pub Signal<u32>);

#[derive(Clone, Copy)]
pub struct SearchPrefill(pub Signal<Option<(String, String)>>);

#[derive(Clone, Copy)]
pub struct AutoDownloadSignal(pub Signal<Option<AutoDownloadEvent>>);
