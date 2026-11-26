//! First-run / server connection setup view for Soulbeet Desktop.
//!
//! Supports both standard Username + Password sign-in (`POST /api/auth/login`) AND
//! direct Token / OpenSubsonic Navidrome setup, verifying credentials live before
//! writing `config.json` (with `0o600` permissions on Unix).

use client_core::models::NavidromeCredentials;
use dioxus::prelude::*;
use ui::{use_player, IconCheck, IconLoader, IconMusic, Logo};

use crate::desktop_state::{
    login_with_password, normalize_auth_token, use_desktop_studio, verify_desktop_config,
    DesktopAuthKind,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupMode {
    PasswordLogin,
    TokenOrNavidrome,
}

#[component]
pub fn SetupView() -> Element {
    let mut studio = use_desktop_studio();
    let player = use_player();
    let initial = studio.config.read().clone();
    let init_sb_url = initial.soulbeet_base_url.clone().unwrap_or_default();
    let init_auth_tok = initial.auth_token.clone().unwrap_or_default();
    let init_auth_kind = initial.auth_kind;
    let init_nav_url = initial
        .navidrome
        .as_ref()
        .map(|n| n.base_url.clone())
        .unwrap_or_default();
    let init_nav_user = initial
        .navidrome
        .as_ref()
        .map(|n| n.username.clone())
        .unwrap_or_default();
    let init_nav_token = initial
        .navidrome
        .as_ref()
        .map(|n| n.subsonic_token.clone())
        .unwrap_or_default();
    let init_nav_salt = initial
        .navidrome
        .as_ref()
        .map(|n| n.subsonic_salt.clone())
        .unwrap_or_default();

    let mut mode = use_signal(|| SetupMode::PasswordLogin);
    let mut soulbeet_url = use_signal(move || init_sb_url);
    let mut login_username = use_signal(String::new);
    let mut login_password = use_signal(String::new);
    let mut auth_token = use_signal(move || init_auth_tok);
    let mut auth_kind = use_signal(move || init_auth_kind);
    let mut nav_url = use_signal(move || init_nav_url);
    let mut nav_user = use_signal(move || init_nav_user);
    let mut nav_token = use_signal(move || init_nav_token);
    let mut nav_salt = use_signal(move || init_nav_salt);
    let mut verifying = use_signal(|| false);
    let mut form_error = use_signal(|| None::<String>);

    let core_err = studio.core_error.read().clone();

    rsx! {
        div { class: "h-screen w-screen bg-beet-canvas text-beet-text-primary flex items-center justify-center p-6 overflow-y-auto studio-scrollbar",
            div { class: "w-full max-w-xl rounded-[16px] bg-beet-surface border border-beet-border-default p-6 sm:p-8 space-y-6",
                // Header
                div { class: "flex items-center gap-3.5",
                    Logo { size: 48 }
                    div {
                        h1 { class: "text-[24px] font-bold tracking-[-0.02em] text-beet-text-primary leading-tight",
                            "Soulbeet Desktop Studio"
                        }
                        p { class: "text-[13px] text-beet-text-secondary mt-0.5",
                            "Offline-first native playback powered by libmpv + local SQLite mirror."
                        }
                    }
                }

                // Mode Switcher
                div { class: "inline-flex items-center gap-1 p-1 rounded-[10px] bg-beet-surface-low border border-beet-border-subtle",
                    button {
                        r#type: "button",
                        class: if mode() == SetupMode::PasswordLogin {
                            "h-8 px-3.5 rounded-[7px] bg-beet-surface-high border border-beet-border-default text-beet-text-primary text-[13px] font-semibold cursor-pointer"
                        } else {
                            "h-8 px-3.5 rounded-[7px] text-beet-text-secondary hover:text-beet-text-primary text-[13px] font-medium cursor-pointer"
                        },
                        onclick: move |_| {
                            form_error.set(None);
                            mode.set(SetupMode::PasswordLogin);
                        },
                        "Sign In"
                    }
                    button {
                        r#type: "button",
                        class: if mode() == SetupMode::TokenOrNavidrome {
                            "h-8 px-3.5 rounded-[7px] bg-beet-surface-high border border-beet-border-default text-beet-text-primary text-[13px] font-semibold cursor-pointer"
                        } else {
                            "h-8 px-3.5 rounded-[7px] text-beet-text-secondary hover:text-beet-text-primary text-[13px] font-medium cursor-pointer"
                        },
                        onclick: move |_| {
                            form_error.set(None);
                            mode.set(SetupMode::TokenOrNavidrome);
                        },
                        "Token / Direct Navidrome"
                    }
                }

                if let Some(err) = form_error().or(core_err) {
                    div { class: "p-3 rounded-[10px] bg-beet-crimson-surface border border-beet-crimson-text/30 text-[13px] text-beet-crimson-text",
                        "{err}"
                    }
                }

                // Soulbeet Server URL (shared across tabs)
                div { class: "space-y-2.5",
                    label { class: "block space-y-1",
                        span { class: "text-[12px] text-beet-text-secondary", "Soulbeet Server URL" }
                        input {
                            r#type: "url",
                            value: "{soulbeet_url}",
                            placeholder: "https://soulbeet.example.com",
                            class: "w-full h-10 px-3 rounded-[8px] bg-beet-surface-low border border-beet-border-default text-[14px] text-beet-text-primary placeholder:text-beet-text-disabled focus:border-beet-leaf-solid outline-none",
                            oninput: move |evt| soulbeet_url.set(evt.value()),
                        }
                    }

                    if mode() == SetupMode::PasswordLogin {
                        div { class: "grid grid-cols-1 sm:grid-cols-2 gap-2.5",
                            label { class: "block space-y-1",
                                span { class: "text-[12px] text-beet-text-secondary", "Username" }
                                input {
                                    r#type: "text",
                                    value: "{login_username}",
                                    placeholder: "alice",
                                    autocomplete: "username",
                                    class: "w-full h-10 px-3 rounded-[8px] bg-beet-surface-low border border-beet-border-default text-[14px] text-beet-text-primary placeholder:text-beet-text-disabled focus:border-beet-leaf-solid outline-none",
                                    oninput: move |evt| login_username.set(evt.value()),
                                }
                            }
                            label { class: "block space-y-1",
                                span { class: "text-[12px] text-beet-text-secondary", "Password" }
                                input {
                                    r#type: "password",
                                    value: "{login_password}",
                                    placeholder: "••••••••",
                                    autocomplete: "current-password",
                                    class: "w-full h-10 px-3 rounded-[8px] bg-beet-surface-low border border-beet-border-default text-[14px] text-beet-text-primary placeholder:text-beet-text-disabled focus:border-beet-leaf-solid outline-none",
                                    oninput: move |evt| login_password.set(evt.value()),
                                }
                            }
                        }
                        p { class: "text-[12px] text-beet-text-tertiary",
                            "Signs in via /api/auth/login and automatically bootstraps scoped Navidrome OpenSubsonic credentials."
                        }
                    } else {
                        div { class: "grid grid-cols-1 sm:grid-cols-3 gap-2.5",
                            label { class: "sm:col-span-2 block space-y-1",
                                span { class: "text-[12px] text-beet-text-secondary", "Auth Token / Session Cookie" }
                                input {
                                    r#type: "password",
                                    value: "{auth_token}",
                                    placeholder: "Bearer token or auth_token cookie value",
                                    class: "w-full h-10 px-3 rounded-[8px] bg-beet-surface-low border border-beet-border-default text-[14px] text-beet-text-primary placeholder:text-beet-text-disabled focus:border-beet-leaf-solid outline-none",
                                    oninput: move |evt| auth_token.set(evt.value()),
                                }
                            }
                            label { class: "block space-y-1",
                                span { class: "text-[12px] text-beet-text-secondary", "Auth Header" }
                                select {
                                    class: "w-full h-10 px-2.5 rounded-[8px] bg-beet-surface-low border border-beet-border-default text-[13px] text-beet-text-primary outline-none",
                                    value: match auth_kind() {
                                        DesktopAuthKind::Bearer => "bearer",
                                        DesktopAuthKind::SessionCookie => "cookie",
                                    },
                                    onchange: move |evt| {
                                        auth_kind.set(if evt.value() == "cookie" {
                                            DesktopAuthKind::SessionCookie
                                        } else {
                                            DesktopAuthKind::Bearer
                                        });
                                    },
                                    option { value: "bearer", "Bearer" }
                                    option { value: "cookie", "Session Cookie" }
                                }
                            }
                        }

                        // Direct Navidrome OpenSubsonic Section
                        div { class: "pt-4 border-t border-beet-border-subtle space-y-3",
                            div { class: "flex items-center justify-between",
                                h2 { class: "text-[13px] font-semibold text-beet-text-primary",
                                    "Navidrome OpenSubsonic (Optional Direct Override)"
                                }
                                span { class: "tabular-nums-mono text-[10px] uppercase tracking-wider text-beet-text-tertiary",
                                    "Direct Stream & Sync"
                                }
                            }

                            div { class: "grid grid-cols-1 sm:grid-cols-2 gap-2.5",
                                label { class: "block space-y-1",
                                    span { class: "text-[12px] text-beet-text-secondary", "Navidrome URL" }
                                    input {
                                        r#type: "url",
                                        value: "{nav_url}",
                                        placeholder: "https://navidrome.example.com",
                                        class: "w-full h-9 px-3 rounded-[8px] bg-beet-surface-low border border-beet-border-default text-[13px] text-beet-text-primary placeholder:text-beet-text-disabled outline-none",
                                        oninput: move |evt| nav_url.set(evt.value()),
                                    }
                                }
                                label { class: "block space-y-1",
                                    span { class: "text-[12px] text-beet-text-secondary", "Username" }
                                    input {
                                        r#type: "text",
                                        value: "{nav_user}",
                                        placeholder: "listener",
                                        class: "w-full h-9 px-3 rounded-[8px] bg-beet-surface-low border border-beet-border-default text-[13px] text-beet-text-primary placeholder:text-beet-text-disabled outline-none",
                                        oninput: move |evt| nav_user.set(evt.value()),
                                    }
                                }
                                label { class: "block space-y-1",
                                    span { class: "text-[12px] text-beet-text-secondary", "Subsonic Token (md5)" }
                                    input {
                                        r#type: "password",
                                        value: "{nav_token}",
                                        placeholder: "Subsonic t= token",
                                        class: "w-full h-9 px-3 rounded-[8px] bg-beet-surface-low border border-beet-border-default text-[13px] text-beet-text-primary placeholder:text-beet-text-disabled outline-none",
                                        oninput: move |evt| nav_token.set(evt.value()),
                                    }
                                }
                                label { class: "block space-y-1",
                                    span { class: "text-[12px] text-beet-text-secondary", "Subsonic Salt" }
                                    input {
                                        r#type: "text",
                                        value: "{nav_salt}",
                                        placeholder: "Subsonic s= salt",
                                        class: "w-full h-9 px-3 rounded-[8px] bg-beet-surface-low border border-beet-border-default text-[13px] text-beet-text-primary placeholder:text-beet-text-disabled outline-none",
                                        oninput: move |evt| nav_salt.set(evt.value()),
                                    }
                                }
                            }
                        }
                    }
                }

                // Actions
                div { class: "flex items-center justify-between gap-3 pt-2",
                    div { class: "flex items-center gap-2 text-[12px] text-beet-text-tertiary",
                        IconMusic { size: 15, class: "text-beet-leaf-text".to_string() }
                        span { "Gapless libmpv · ReplayGain · 0600 Config" }
                    }
                    button {
                        r#type: "button",
                        disabled: verifying(),
                        class: "inline-flex items-center gap-2 h-10 px-5 rounded-full bg-beet-leaf-solid text-beet-leaf-ink text-[14px] font-semibold hover:brightness-105 disabled:opacity-50 cursor-pointer transition-[filter]",
                        onclick: move |_| {
                            if verifying() {
                                return;
                            }
                            let current_mode = mode();
                            let sb_url = soulbeet_url().trim().trim_end_matches('/').to_string();
                            let uname = login_username().trim().to_string();
                            let pass = login_password();
                            let raw_tok = auth_token();
                            let akind = auth_kind();
                            let nu = nav_url().trim().trim_end_matches('/').to_string();
                            let nuser = nav_user().trim().to_string();
                            let ntok = nav_token().trim().to_string();
                            let nsalt = nav_salt().trim().to_string();
                            let base_cfg = studio.config.peek().clone();

                            form_error.set(None);
                            verifying.set(true);

                            spawn(async move {
                                let mut next = base_cfg;
                                if current_mode == SetupMode::PasswordLogin {
                                    if sb_url.is_empty() || uname.is_empty() || pass.is_empty() {
                                        form_error.set(Some(
                                            "Enter your Soulbeet URL, username, and password."
                                                .to_string(),
                                        ));
                                        verifying.set(false);
                                        return;
                                    }
                                    match login_with_password(&sb_url, &uname, &pass).await {
                                        Ok(res) => {
                                            if res.password_change_required {
                                                form_error.set(Some(
                                                    "This account still uses the default password. Change it in the Soulbeet web UI first, then sign in here."
                                                        .to_string(),
                                                ));
                                                verifying.set(false);
                                                return;
                                            }
                                            next.soulbeet_base_url = Some(sb_url);
                                            next.auth_kind = DesktopAuthKind::SessionCookie;
                                            next.auth_token = Some(res.session_cookie);
                                            next.navidrome = None;
                                        }
                                        Err(err) => {
                                            form_error.set(Some(err));
                                            verifying.set(false);
                                            return;
                                        }
                                    }
                                } else {
                                    let normalized_tok = normalize_auth_token(&raw_tok, akind);
                                    next.soulbeet_base_url =
                                        (!sb_url.is_empty()).then_some(sb_url);
                                    next.auth_token =
                                        (!normalized_tok.is_empty()).then_some(normalized_tok);
                                    next.auth_kind = akind;
                                    next.navidrome = if !nu.is_empty() && !nuser.is_empty() {
                                        Some(NavidromeCredentials {
                                            base_url: nu,
                                            username: nuser,
                                            subsonic_salt: nsalt,
                                            subsonic_token: ntok,
                                            api_version: "1.16.1".to_string(),
                                            client_name: "Soulbeet".to_string(),
                                        })
                                    } else {
                                        None
                                    };
                                }

                                if !next.is_configured() {
                                    form_error.set(Some(
                                        "Enter a Soulbeet URL + credentials, or direct Navidrome credentials."
                                            .to_string(),
                                    ));
                                    verifying.set(false);
                                    return;
                                }

                                // Verify credentials live before persisting config & entering Studio.
                                match verify_desktop_config(&next).await {
                                    Ok(boot) => {
                                        verifying.set(false);
                                        studio.connect_with_config(next, boot, player);
                                    }
                                    Err(err) => {
                                        form_error.set(Some(err));
                                        verifying.set(false);
                                    }
                                }
                            });
                        },
                        if verifying() {
                            IconLoader { size: 16 }
                            span { "Verifying…" }
                        } else {
                            IconCheck { size: 16 }
                            span { "Connect & Sync Library" }
                        }
                    }
                }
            }
        }
    }
}
