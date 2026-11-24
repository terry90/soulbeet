use dioxus::prelude::*;
use shared::metadata::Album;

fn get_album_cover_url(album: &Album) -> Option<String> {
    if let Some(url) = &album.cover_url {
        return Some(url.clone());
    }
    album
        .mbid
        .as_ref()
        .map(|mbid| format!("https://coverartarchive.org/release/{}/front-250", mbid))
}

#[component]
pub fn CoverArt(album: Album) -> Element {
    let mut has_error = use_signal(|| false);
    let cover_url = get_album_cover_url(&album);
    let alt = format!("Cover for {}", album.title);

    rsx! {
      div { class: "w-20 h-20 flex-shrink-0 bg-beet-panel border border-white/5 rounded-md flex items-center justify-center overflow-hidden",
        if let Some(url) = cover_url.filter(|_| !has_error()) {
          img {
            src: "{url}",
            alt: "{alt}",
            class: "w-full h-full object-cover",
            // An artist's albums can list hundreds of rows.
            loading: "lazy",
            decoding: "async",
            onerror: move |_| has_error.set(true),
          }
        } else {
          svg {
            class: "w-8 h-8 text-white/20",
            xmlns: "http://www.w3.org/2000/svg",
            fill: "none",
            "viewBox": "0 0 24 24",
            "stroke-width": "1.5",
            stroke: "currentColor",
            path {
              "stroke-linecap": "round",
              "stroke-linejoin": "round",
              d: "M9 9l10.5-3m0 6.553v3.75a2.25 2.25 0 01-1.632 2.163l-1.32.377a1.803 1.803 0 11-.99-3.467l2.31-.66a2.25 2.25 0 001.632-2.163zm0 0V2.25L9 5.25v10.303m0 0v3.75a2.25 2.25 0 01-1.632 2.163l-1.32.377a1.803 1.803 0 01-.99-3.467l2.31-.66A2.25 2.25 0 009 15.553z",
            }
          }
        }
      }
    }
}
