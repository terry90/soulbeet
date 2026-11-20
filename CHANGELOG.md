# Changelog

## Unreleased

### Discovery without Last.fm

Discovery and search no longer use Last.fm.

- **Discovery needs no key**: Recommendations now come from what you play and star in Navidrome, matched against ListenBrainz and MusicBrainz data; no account or API key is needed, and a ListenBrainz username adds your listens there.
- **Optional Deezer source**: A switch in Settings > Library adds tracks from Deezer artist radios; it is off by default because it sends your artist names to Deezer.
- **Import your Last.fm history now**: Last.fm keys stop working on December 1 unless their owner pays for Pro, so bring your scrobbles into ListenBrainz before then with its importer at https://listenbrainz.org/settings/import/, which needs only your Last.fm username.
- **Last.fm is gone**: Search uses MusicBrainz only and the Last.fm settings are removed; a download left from before the upgrade whose album came from a Last.fm search fails, and the log and `get_jobs` say to search for it again.
- **For AI assistants**: `search_music` no longer takes `provider`, and the `provider_not_configured` error is gone; see Upgrading in docs/mcp.md.

## 0.9.0 - 2026-11-19

### Ask your AI assistant for music

Connect Claude, ChatGPT or another AI assistant to Soulbeet, then ask for music in a chat.

- **Ask in plain words**: Ask for an album or a whole playlist, and it downloads what you are missing and builds the playlist in Navidrome.
- **It keeps going**: Downloads carry on after you close the chat.
- **Deleting is off**: An assistant cannot delete music until you allow it in Settings > Users > Connected apps.
- **You were signed out**: This update signs everyone out once, so sign in again.
- **For admins**: Only the `admin` account keeps admin rights; see Upgrading in the README.

[How to set it up](https://github.com/terry90/soulbeet#mcp-server)
