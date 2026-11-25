# Changelog

## Unreleased

### Search by artist, a new search bar, and smoother on phones

An artist alone now lists their albums, the search bar is rebuilt as one layout for every screen, and phones get fixes for lag, zoom and sign-in.

- **Search by artist**: Fill in only the artist to list their albums, EPs and singles, studio albums first, with the year and type on each row.
- **New search bar**: The artist filter opens with a tap on Artist, the download folder can be picked on phones too, and the three service lights are now one status line that opens to the details.
- **Lighter pages**: The background glows are drawn without a blur filter, which Safari on iOS 26 could stall on for seconds, and the closed downloads drawer is no longer kept on screen.
- **Typing and sign-in on phones**: Fields no longer make Safari zoom in, and password managers can fill and save the login and password forms.
- **Faster first load**: The app is sent compressed, at about a third of its size.

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
