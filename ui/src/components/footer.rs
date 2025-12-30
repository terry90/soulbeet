use dioxus::fullstack::reqwest;
use dioxus::prelude::*;

fn extract_package_version(toml_content: &str) -> Option<String> {
    for line in toml_content.lines() {
        let line = line.trim();
        if line.starts_with("version") {
            let parts: Vec<&str> = line.splitn(2, '=').collect();
            if parts.len() == 2 {
                let version = parts[1].trim().trim_matches('"');
                return Some(version.to_owned());
            }
        }
    }
    None
}

#[component]
pub fn Footer() -> Element {
    let cargo_toml = include_str!("../../Cargo.toml");
    let local_version = extract_package_version(cargo_toml);

    let remote_version = use_resource(|| async move {
        let url =
            "https://raw.githubusercontent.com/terry90/soulbeet/refs/heads/master/api/Cargo.toml";
        match reqwest::get(url).await {
            Ok(resp) => {
                if resp.status().is_success() {
                    match resp.text().await {
                        Ok(text) => extract_package_version(&text),
                        Err(_) => None,
                    }
                } else {
                    None
                }
            }
            Err(_) => None,
        }
    });

    let version = match (&*remote_version.read_unchecked(), local_version) {
        (Some(Some(remote)), Some(ref local)) => {
            if remote == local {
                // Up to date
                rsx! {
                  a {
                    href: "https://hub.docker.com/repository/docker/docccccc/soulbeet",
                    target: "_blank",
                    class: "text-green-800",
                    "[ v{local} • up to date ]"
                  }
                }
            } else {
                // Newer version available
                rsx! {
                  a {
                    href: "https://hub.docker.com/repository/docker/docccccc/soulbeet",
                    target: "_blank",
                    class: "text-amber-800 hover:text-amber-500 transition-colors cursor-pointer",
                    title: "Newer version {remote} available",
                    "[ v{local} -> v{remote} ]"
                  }
                }
            }
        }
        (Some(None), Some(ref local)) => {
            // Failed to parse remote version
            rsx! {
              span { class: "text-gray-500", "[ v{local} ]" }
            }
        }
        (None, Some(ref local)) => {
            // Loading
            rsx! {
              span { class: "text-cyan-800", "[ v{local} • checking... ]" }
            }
        }
        _ => {
            // No local version found
            rsx! {
              span { class: "text-red-800", "[ version check failed ]" }
            }
        }
    };

    rsx! {
      footer { class: "py-4 text-center border-t border-white/5",
        div { class: "flex justify-center gap-6 text-[10px] font-mono uppercase tracking-widest text-gray-500",
          {version}
          a {
            class: "hover:text-beet-accent transition-colors",
            href: "https://hub.docker.com/repository/docker/docccccc/soulbeet",
            target: "_blank",
            "[ Docker Hub ]"
          }
          a {
            class: "hover:text-beet-accent transition-colors",
            href: "https://github.com/terry90/soulbeet",
            target: "_blank",
            "[ Github ]"
          }
        }
      }
    }
}
