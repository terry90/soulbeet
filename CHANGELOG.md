# Changelog

## 1.0.0 - 2026-11-26

### Built-in player, Desktop and Android Auto apps, Discovery 2.0, and five download backends

Soulbeet now plays your Navidrome library directly on Web, Linux, Windows, Android, and Android Auto, alongside Discovery 2.0, five download sources, and admin MCP library tools.

- **Studio Web player and native clients**: Stream straight from Navidrome with a precision scrubber, synced LRCLIB lyrics, heart favorites separate from 5-star ratings, queue and stats views, Soulbeet Connect device handoff, plus native Linux, Windows, Android, and Android Auto apps with offline caching.
- **Discovery 2.0 and playlists**: Mix your library with fresh picks using Moment Mixes, Seasonal Packs with cultural steering, Instant Seed Radio, and a fast Triage Deck with undo, plus separated Soulbeet Mixes and accurate smart playlist track counts.
- **Five download backends and 1-tap Acquire**: Search your library and MusicBrainz together across Soulseek, Internet Archive, YouTube Music, NetEase, and Direct Connect, play unowned tracks with 1-tap Acquire and Play, and auto-upgrade kept lossy tracks to FLAC in the background.
- **Smarter AI assistant (MCP)**: Browse, inspect, retag, audit, and move tracks across libraries, including multi-account admin access (`allow_admin` in Settings > Users > Connected apps) so your assistant can move tracks between personal and shared folders.
- **Smoother mobile and faster loading**: Compressed self-hosted fonts (`Space Grotesk` and `Commit Mono`), lighter UI without Safari blur stalls, full artist discographies, and prebuilt client binaries attached to GitHub Releases.

[Clients setup guide](https://github.com/terry90/soulbeet/blob/master/docs/clients.md)

## 0.10.0 - 2026-11-22

### Discovery without Last.fm, and single sign-on

Discovery and search no longer use Last.fm, and you can sign in through an OpenID Connect provider such as Pocket ID, Authelia, Authentik or Keycloak.

- **Discovery needs no key**: Recommendations now come from what you play and star in Navidrome, matched against ListenBrainz and MusicBrainz data; no account or API key is needed, a ListenBrainz username adds your listens there, and a switch in Settings > Library adds tracks from Deezer artist radios, off by default because it sends your artist names to Deezer.
- **Import your Last.fm history now**: Last.fm keys stop working on December 1 unless their owner pays for Pro, so bring your scrobbles into ListenBrainz before then with its importer at https://listenbrainz.org/settings/import/, which needs only your Last.fm username.
- **Last.fm is gone**: Search uses MusicBrainz only and the Last.fm settings are removed; a download left from before the upgrade whose album came from a Last.fm search fails, and the log and `get_jobs` say to search for it again; for AI assistants `search_music` no longer takes `provider`, see Upgrading in docs/mcp.md.
- **Sign in with your login provider**: Once an admin sets it up, the login page has a button to sign in through your provider, and an account made that way connects to Navidrome when you enter your Navidrome password in Connect Navidrome in Settings > Users.
- **Same name, same account**: A first sign-in joins the plain user account with the same name, never an admin account (an admin links single sign-on in Settings > Users), so set OIDC_LINK_BY_USERNAME=false if the names at your provider are not under your control, and linking from Settings > Users still asks for your current password and shows the name you linked.

[How to set up single sign-on](https://github.com/terry90/soulbeet/blob/master/docs/sso.md)

## 0.9.0 - 2026-11-19

### Ask your AI assistant for music

Connect Claude, ChatGPT or another AI assistant to Soulbeet, then ask for music in a chat.

- **Ask in plain words**: Ask for an album or a whole playlist, and it downloads what you are missing and builds the playlist in Navidrome.
- **It keeps going**: Downloads carry on after you close the chat.
- **Deleting is off**: An assistant cannot delete music until you allow it in Settings > Users > Connected apps.
- **You were signed out**: This update signs everyone out once, so sign in again.
- **For admins**: Only the `admin` account keeps admin rights; see Upgrading in the README.

[How to set it up](https://github.com/terry90/soulbeet#mcp-server)
