# Privacy Policy for Soulbeet

**Effective date:** November 26, 2026  
**Application:** Soulbeet (`soulbeet.insynia.fr`)  
**Developer contact:** contact@insynia.fr  
**Source code:** https://github.com/terry90/soulbeet

## Overview

Soulbeet is an open-source, self-hosted music client for Android and Android Auto. It connects exclusively to the Soulbeet and Navidrome servers that you host and configure yourself.

The developer does **not** operate any central backend for the app, does **not** run analytics or advertising SDKs, and does **not** collect, store, sell, or share any personal data.

## Data Handled by the App

To function as a client for your own server, the app handles the following information strictly between your device and the server URL you enter:

- **Server connection and authentication:** Your self-hosted server URL, username, and session tokens. On Android, session credentials are encrypted on-device using Android Keystore (AES-256-GCM) and stored locally.
- **Music library metadata and offline cache:** Track, album, artist, and playlist metadata mirrored into a local on-device SQLite database, along with cached audio files and cover art for offline playback and Android Auto browsing.
- **Playback state and curation actions:** Ratings, favorites, play history, and Soulbeet Connect device presence are synced only with your configured self-hosted Soulbeet and Navidrome servers.
- **Voice search (Android Auto):** Voice queries handled by Android Auto are resolved locally against the on-device library index or sent to your configured self-hosted server.

None of this data is transmitted to the developer or to third-party trackers.

## Third-Party Services

The Android app itself does not embed third-party analytics, crash-reporting services, or ad networks. Any external metadata, lyrics, or download lookups (such as MusicBrainz, ListenBrainz, or LRCLIB) are configured and performed by your own self-hosted server according to your server settings.

## Data Retention and Deletion

Because all data lives either locally on your device or on your own self-hosted server:

- Signing out of the app or clearing app storage removes locally cached credentials, library data, and offline audio from your device.
- Uninstalling the app deletes all local app data from your device.
- Server-side data is controlled entirely by the operator of the self-hosted Soulbeet and Navidrome server you connect to.

## Children's Privacy

Soulbeet does not knowingly collect personal information from anyone, including children.

## Changes to This Policy

If this privacy policy is updated, the latest version will always be published at this URL with an updated effective date.

## Contact

If you have questions about this privacy policy, email **contact@insynia.fr** or open an issue at https://github.com/terry90/soulbeet/issues.
