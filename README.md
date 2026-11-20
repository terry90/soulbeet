# Soulbeet

[![Docker Pulls](https://img.shields.io/docker/pulls/docccccc/soulbeet)](https://hub.docker.com/r/docccccc/soulbeet)
[![Docker Image Size](https://img.shields.io/docker/image-size/docccccc/soulbeet)](https://hub.docker.com/r/docccccc/soulbeet/tags)
[![Docker Image Version](https://img.shields.io/docker/v/docccccc/soulbeet)](https://hub.docker.com/r/docccccc/soulbeet/tags)

[![GitHub Actions Workflow Status](https://img.shields.io/github/actions/workflow/status/terry90/soulbeet/image-build-push.yml)](https://github.com/terry90/soulbeet/actions)
[![GitHub License](https://img.shields.io/github/license/terry90/soulbeet)](https://github.com/terry90/soulbeet)
[![GitHub Repo stars](https://img.shields.io/github/stars/terry90/soulbeet)](https://github.com/terry90/soulbeet)

Soulbeet is a self-hosted music downloader, library manager, and discovery engine. Search for music, download it, and let the app handle everything else: finding the best source, tagging, organizing, and keeping your library clean. Play music in Navidrome and Soulbeet will find new music for you automatically. Or skip the UI and ask your AI assistant: Soulbeet is an [MCP server](#mcp-server).

> "Make a Christmas playlist with 30 classics and download the ones I don't have."

Screenshots: [here](./screenshots)

## Features

- **Ask your AI assistant**: Soulbeet is an MCP server. Connect Claude, ChatGPT or any other MCP client and ask for an album or a whole playlist in plain words. It finds the tracks, downloads what is missing and builds the playlist in Navidrome. See [MCP Server](#mcp-server).
- **Search & Download**: Find albums and tracks, hit download. Soulbeet picks the best available source from Soulseek, downloads it, tags it with beets, and puts it in your library. No manual file management.
- **Music Discovery**: Soulbeet reads what you play and star in Navidrome, adds your ListenBrainz history if you give it your username, finds new music through similar artists, collaborative filtering and genre exploration (and Deezer artist radios if you turn them on), downloads the best candidates, and pushes playlists to your Navidrome server. Fully automatic, and Navidrome is the only account it needs.
- **Three Discovery Profiles**: Conservative (close to what you know), Balanced, or Adventurous (unfamiliar territory). Run one or all three, each with its own playlist.
- **Rate & Keep**: Listen in Navidrome. 3+ stars promotes a track to your permanent library, 1 star deletes it. Unrated tracks expire and get replaced with fresh picks.
- **Multi-user**: Private or shared folders. Each user gets their own discovery profiles, ListenBrainz username, and preferences.

## How It Works

You search and click download, or you ask your assistant. Behind the scenes:

1. **Soulbeet** finds the best source on Soulseek, downloads it through **slskd**, tags and organizes it with **beets**, and puts it in your library.
2. **Navidrome** picks up the new files and makes them streamable.
3. For discovery: your **Navidrome** plays and stars (and your **ListenBrainz** history, when you saved a username) feed the recommendation engine. It asks ListenBrainz for similar artists and MusicBrainz for artist ids and genres, finds new tracks, downloads them, and pushes playlists to Navidrome.

## MCP Server

Soulbeet is an MCP server, so any AI assistant that speaks MCP (Claude, ChatGPT, Cursor, your own agent...) can drive it. You ask for music in plain words. The assistant checks your library, searches Soulseek, downloads, imports and builds the Navidrome playlist, and you never open the web UI.

The endpoint is `{your Soulbeet address}/mcp`. It speaks streamable HTTP and signs clients in with OAuth, so there is no API key to copy around.

### Set it up

1. Get Soulbeet running (see [Self-Hosting with Docker](#self-hosting-with-docker)) and serve it over https, at the root of the name: `https://music.example.com` works, `https://example.com/music` does not. Hosted assistants (claude.ai, ChatGPT) connect from their own servers, so for those the address must be public.
2. Have your reverse proxy send the public host and `X-Forwarded-Proto: https`, or set `PUBLIC_URL`.
3. Open Settings > Config at that address. The Security Checks must pass (`SECRET_KEY` unset or at least 32 bytes, `admin` password changed), and **Remote access** tells you whether the address is usable.
4. Add `https://music.example.com/mcp` as a remote MCP server in your client. On claude.ai, for example, that is **Customize > Connectors > Add custom connector**. The client sends you to Soulbeet: sign in and click **Allow**.

Each person connects their own account, and the assistant acts as that person only.

### Use it

Ask in a chat, from your desk or your phone:

- "Make a Christmas playlist with 30 classics and download the ones I don't have."
- "Get In Rainbows by Radiohead into my library."
- "How is that batch of downloads going?"
- "Do I have duplicates in my library?"

Downloads keep running after you close the chat, and a playlist asked for with them keeps filling. Ask about them later in a new chat.

### Deleting music

A connected app cannot delete anything until you tick **Allow deleting music** for it in Settings > Users > Connected apps. With it on, the assistant shows what it would delete and waits for your yes before deleting tracks. It can then also run your rating sync, which deletes your 1-star songs when auto-delete is on. A track another listener rated above 1 star, starred or keeps in a playlist is kept, and the limit is 100 tracks per 24 hours.

To turn the MCP server off, set `MCP=off`.

[docs/mcp.md](docs/mcp.md) has the proxy examples (Caddy, nginx, Traefik, Cloudflare tunnel), every tool, the limits and the error codes. It walks through claude.ai, the client it was tested with.

## Self-Hosting with Docker

The recommended way to run Soulbeet is via Docker Compose. This ensures all dependencies (like `beets` and `python`) are correctly set up.

**Compatibility:** The Docker image supports both **AMD64** and **ARM64** architectures.

### Prerequisites

-   Docker & Docker Compose (or podman-compose)

### Quick Start

1.  Create a `docker-compose.yml` file:

```yaml
services:
  soulbeet:
    image: docker.io/docccccc/soulbeet:latest
    restart: unless-stopped
    ports:
      - 9765:9765
    environment:
      - DATABASE_URL=sqlite:/data/soulbeet.db
      - DOWNLOAD_PATH=/downloads
      # Optional, see step 2. Passed on only when it is set.
      - SECRET_KEY
      - NAVIDROME_URL=http://navidrome:4533
      # Optional
      - BEETS_CONFIG=/config/config.yaml
    volumes:
      - ./data:/data
      - ./beets:/config
      - /path/to/slskd/downloads:/downloads
      - /path/to/music:/music
    depends_on:
      - slskd
      - navidrome

  navidrome:
    image: deluan/navidrome:latest
    ports:
      - "4533:4533"
    environment:
      - ND_MUSICFOLDER=/music
    volumes:
      - ./navidrome-data:/data
      - /path/to/music:/music

  slskd:
    image: slskd/slskd
    environment:
      - SLSKD_REMOTE_CONFIGURATION=true
    volumes:
      - ./slskd-config:/app/slskd.conf.d
      - /path/to/slskd/downloads:/app/downloads
    ports:
      - "5030:5030"
```

2.  The secret key that signs sessions and encrypts stored Navidrome passwords is optional. Without `SECRET_KEY`, Soulbeet generates one on the first start and stores it in `secret.key` next to the database (in the `./data` volume above). Back up that file together with the database: losing or changing the key signs everyone out and makes the stored Navidrome passwords unreadable until each user enters theirs again, by signing in with it or under Connect Navidrome in Settings > Users. To set your own key instead, put a random value of at least 32 bytes in a `.env` file next to `docker-compose.yml`. A shorter key is not used: Soulbeet signs with the generated key instead, and Settings > Config lists it under Security Checks until you remove it or set a longer one. Skip this if your `.env` already sets `SECRET_KEY`; the command adds a line and keeps whatever the file already holds:

```bash
echo "SECRET_KEY=$(openssl rand -hex 32)" >> .env
```

3.  Seed the beets config. `BEETS_CONFIG` replaces the config shipped in the image, so start from a copy of it. Use the same tag as the `image:` line above, since the shipped `plugins:` line depends on the tier (see [Image Tiers](#image-tiers)), and copy it again after you change tier:

```bash
TAG=latest # the tag on your image: line (latest, light, medium or full)
mkdir -p beets
docker create --name sb "docker.io/docccccc/soulbeet:$TAG"
docker cp sb:/app/beets_config.yaml ./beets/config.yaml
docker rm sb
```

4.  **Important**: The `/downloads` volume must match between `slskd` and `soulbeet` so Soulbeet can see the files `slskd` downloaded. The `/music` volume must match between `soulbeet` and `navidrome` so Navidrome can see the organized library. Soulbeet requires **slskd 0.26 or newer** (downloads are enqueued through the batches API introduced in 0.26).

5.  Run:

```bash
docker-compose up -d
```

### Initial Setup

A fresh install has one account, `admin` with the password `admin`. It is the only admin. Everyone sees the Settings tabs, but only admins see user management in Settings > Users, the slskd settings and Security Checks in Settings > Config, and the folder administration in Settings > Library.

1.  Open `http://localhost:9765` and sign in as `admin` / `admin`, with or without Navidrome.
2.  In **Settings > Users**, change the admin password. Until you do, Settings > Config lists it under Security Checks.
3.  In **Settings > Config**, connect slskd (URL + API key). [How to get an slskd API key](https://github.com/slskd/slskd/blob/master/docs/config.md#yaml-24).
4.  In **Settings > Library**, add your music folders (e.g. `/music`). Admins can add a folder anywhere.
5.  Let the other users in:
    - **With Navidrome** (`NAVIDROME_URL` set): everyone signs in with their **Navidrome credentials**, and their Soulbeet account is created on the first sign-in. To make your own Navidrome account an admin, sign in with it once, then sign in as `admin` and click **Make admin** next to it in Settings > Users.
    - **Without Navidrome**: create the accounts in Settings > Users.
6.  Users who are not admins can only add folders inside a folder root an admin approved. Add one under **Approved Folder Roots** in Settings > Library, for example `/music`, so a user can add `/music/alice`. The root itself cannot be the folder of a user who is not an admin. If one of your own folders is that root (an admin folder at `/music`), tick **Shared** next to it, or every folder inside it is refused as being inside another user's folder.
7.  That's it. Search for something and download it.

For discovery (optional): enable discovery on a folder in Settings > Library, pick your profiles, and hit Generate. Play or star some music in Navidrome first, since that is what discovery starts from. You can also add your ListenBrainz username there.

## Configuration

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `DATABASE_URL` | Connection string for SQLite | `sqlite:soulbeet.db` |
| `DOWNLOAD_PATH` | Path where slskd saves downloads | `/downloads` |
| `SECRET_KEY` | Signs sessions and encrypts stored Navidrome passwords. Optional: when unset, a key is generated on the first start and stored in `secret.key` next to the database, where it is read on later starts. A key you set of at least 32 bytes (`openssl rand -hex 32`) wins over that file. A shorter one is not used: the file's key signs instead, and Security Checks lists it. Changing the key signs everyone out and makes stored Navidrome passwords unreadable until each user enters theirs again (by signing in with it, or under Connect Navidrome) | generated |
| `NAVIDROME_URL` | Your Navidrome server URL. Optional: without it you log in with local accounts, and discovery playlists, rating sync and auto-delete are unavailable | |
| `REGISTRATION` | Who can create accounts: `navidrome` (accounts appear when a Navidrome user signs in), `admin` (only an admin creates accounts), `open` (anyone can register) | `navidrome` with `NAVIDROME_URL`, else `admin` |
| `BEETS_CONFIG` | Path to your beets config file. Replaces the shipped `/app/beets_config.yaml` (beets defaults still apply underneath) | `beets_config.yaml` |
| `BEETS_PYTHON` | Python interpreter that runs beets imports and the config check. By default it is read from the `#!` line of `beet` (`/usr/bin/python3` in the image) | |
| `PUBLIC_URL` | Optional. The public address (`https://host[:port]`, no path). When set, Soulbeet uses it instead of reading the address from request headers | |
| `MCP` | `on` or `off`, in any case, for the OAuth and `/mcp` endpoints MCP clients use; any other value turns them off and logs a warning | `on` (unset) |
| `NAVIDROME_MUSIC_PATH` | Optional. Where Soulbeet sees the root of Navidrome's default library. Set it when Soulbeet mounts that library at another path than Navidrome does (e.g. Navidrome at `/music`, Soulbeet at `/media/music`): deletion checks, the rating sync and the MCP tools use it to find Navidrome's songs on disk. Without it, Soulbeet learns where each library is from the files it imports | |
| `BEETS_ALBUM_MODE` | Enable album import mode (see below) | `false` |
| `OIDC_ISSUER_URL` | Issuer of your OpenID Connect provider. Setting it turns on single sign-on (see below) | |
| `OIDC_CLIENT_ID` | Client ID registered at the provider | |
| `OIDC_CLIENT_SECRET` | Client secret. Leave unset for a public client (PKCE only) | |
| `OIDC_REDIRECT_URL` | Optional. One callback for every sign-in, `https://<your soulbeet>/api/auth/oidc/callback`. Without it, the provider sends users back to the address they started at (`PUBLIC_URL` when set) | |
| `OIDC_PROVIDER_NAME` | Name on the login button ("Sign in with ...") | `SSO` |
| `OIDC_SCOPES` | Scopes to request (`openid` is always added) | `openid profile email` |
| `OIDC_USERNAME_CLAIM` | ID token claim used as the Soulbeet username | `preferred_username` |
| `OIDC_AUTO_REGISTER` | Create an account on first sign-in when none has that username | `true` with `REGISTRATION=open`, else `false` |
| `OIDC_LINK_BY_USERNAME` | On a first sign-in, link the existing account of that username. Set `false` to refuse such a sign-in instead. Read the warning below | `true` |
| `RUST_LOG` | Log filter. Use `info,soulbeet::beets::output=debug` to log every line beets prints. A bare `soulbeet::beets::output=debug` turns off all other info logs | |

**Note**: For an account linked to Navidrome, Navidrome decides: if it rejects your credentials, the sign-in is refused. Only while Navidrome cannot be reached (or `NAVIDROME_URL` is not set) does Soulbeet check the password stored in its own database, which follows your Navidrome password after each successful Navidrome login. Accounts not linked to Navidrome sign in with their Soulbeet password. A Navidrome login only takes over an existing Soulbeet account that is already linked to Navidrome or whose Soulbeet password is the same.

**Note**: Usernames are unique without regard to letter case, so `Bob` and `bob` are the same name.

**Note**: Users who are not admins can only add library folders inside a root an admin approved in Settings > Library, never at the root itself.

**Note**: slskd URL and API key are configured through the web UI (Settings > Config) and stored in the database. The ListenBrainz username, the optional token and the Deezer switch are configured per-user in Settings > Library.

### Single Sign-On (OpenID Connect)

Soulbeet can sign users in through an OpenID Connect provider such as [Pocket ID](https://pocket-id.org), Authelia, Authentik or Keycloak. The login page then shows a **Sign in with ...** button next to the password form, which keeps working.

1.  At your provider, create a client. The provider sends users back to `/api/auth/oidc/callback` at the address they started from, so register that path for each address people sign in from as a redirect URI, for example `https://soulbeet.example.com/api/auth/oidc/callback` and `http://192.168.1.10:9765/api/auth/oidc/callback`. Register each address exactly, never a wildcard or pattern: Soulbeet builds the address from the request, so a pattern would let a forged request send a sign-in to another site. When Soulbeet can be reached from the internet, set `PUBLIC_URL` or `OIDC_REDIRECT_URL`. With `PUBLIC_URL` or `OIDC_REDIRECT_URL` set, only that address is used: a sign-in started at another address moves there first, and single sign-on is linked in Settings there. Soulbeet always sends PKCE, so the client can be public (no secret) if the provider allows it.
2.  Set the variables, for example:
    ```yaml
    environment:
      - OIDC_ISSUER_URL=https://id.example.com
      - OIDC_CLIENT_ID=<client id>
      - OIDC_CLIENT_SECRET=<client secret>
      - OIDC_PROVIDER_NAME=Pocket ID
    ```
3.  Restart. A configuration error leaves single sign-on off. It is logged at startup and shown in Settings > Config > Security Checks.

How accounts are matched:

-   An account is tied to the provider's user (its issuer and `sub`), so renaming the user at the provider does not change accounts.
-   **The same name joins the same account**: when an account already has the name `OIDC_USERNAME_CLAIM` gives (ignoring letter case, like every sign-in) and no single sign-on link yet, a first sign-in joins that account and links it, which signs the account out of its other sessions. An account linked to another identity is never joined. Read the warning below on when to turn this off.
-   With no account of that name, a first sign-in creates one as a plain user when `OIDC_AUTO_REGISTER` is on. With Navidrome, an account that single sign-on creates takes that name: if the Navidrome user of that name is someone else, they cannot sign in until an admin deletes that account. `OIDC_AUTO_REGISTER` is on by default only with `REGISTRATION=open`. When it is off, the sign-in is refused and the user gets an account the way they would without single sign-on: with `REGISTRATION=navidrome` by signing in once with their Navidrome password, with `REGISTRATION=admin` from an admin in Settings > Users. Their next single sign-on then joins that account, or, with `OIDC_LINK_BY_USERNAME=false`, they sign in with that password and link single sign-on in Settings. Restrict who may sign in at the provider (Pocket ID: allowed user groups on the client).
-   With `OIDC_USERNAME_CLAIM=email`, a sign-in is accepted only when the provider says the address is verified (`email_verified` is `true`). A missing or any other value is refused.
-   **Linking and unlinking**: signed in, a user can also link single sign-on in Settings > Users, for example when their name at the provider is not their Soulbeet name. They enter their current password first, the one the login form takes (their Navidrome password for an account that has only that), so a session left open is not enough to link someone else's identity to the account. Changing their own password asks for the current one the same way, so such a session cannot set a new one first. An account with no password at all gets one from an admin, which also removes its single sign-on link, so the user links it again in Settings after. Linking sends them to the provider and back. Soulbeet asks the provider to make them sign in there again, and Settings then shows the name of the identity linked, so they can check that it is theirs and not a provider session someone else left open in that browser. An identity is linked to one account only, and an account to one identity. A user can unlink their own account there while it keeps another way in (a password of its own, or a stored Navidrome password). An admin sees in Settings > Users which accounts use single sign-on, with the name of the identity at the provider, and which have no password, and can unlink any of them. An admin who sets the password of another account also removes its link, so whoever held that identity is out (see the warning below for a link made by name); a user who changes their own password keeps theirs. Linking, unlinking or a new password signs the account out everywhere else.
-   **Navidrome**: an account created through single sign-on has no password and no Navidrome password yet, and a banner says so. A Navidrome login with the same username does not join it, because the provider and Navidrome may give that name to different people. Instead, the user enters their Navidrome password under **Connect Navidrome** in Settings > Users. Soulbeet checks it with Navidrome for the account's own username and stores it. Any account can do this, also to store a Navidrome password that changed.
-   Signing in from an MCP client's authorization page goes back to it afterwards, as a password sign-in does, also after a failed single sign-on: the error page keeps the request.
-   **Changing provider**: unlink each account in Settings > Users, set a password on the accounts marked as having none, and have users sign in with the new provider, which joins accounts by name, or link it in Settings.

**Warning**: by default, whoever holds a name at the provider gets the Soulbeet account of that name, admin accounts included. Unlinking an account taken this way, or an admin setting its password (which removes the link), only helps once `OIDC_LINK_BY_USERNAME` is `false` or the name at the provider is changed: the next sign-in under that name links it again.

The default is fine for a family, friends or local instance where the admin runs the provider too, creates the accounts there, and people cannot pick or change their own username at the provider. With `REGISTRATION=navidrome`, the admin also creates the Navidrome accounts, so the names match there too. Then the same name means the same person, and existing Soulbeet users just click **Sign in with ...**.

Set `OIDC_LINK_BY_USERNAME=false` if any of these is true:

-   People can sign up at the provider by themselves, or change their username there (or the claim `OIDC_USERNAME_CLAIM` names).
-   The provider is shared with people who should not all get into Soulbeet, such as a company or public provider.
-   Soulbeet runs with `REGISTRATION=open`, or strangers can create accounts in it some other way. Someone can register a name with a password first; the real person's first single sign-on then joins that account, and the stranger keeps the password.
-   Soulbeet runs with `REGISTRATION=navidrome`, and a Navidrome user may have the same name as a different person at the provider. A Navidrome user who signed in to Soulbeet once has an account, and the first single sign-on under that name joins it, with that user's Navidrome library, ratings and playlists.
-   Someone at the provider holds the name of a Soulbeet admin (the seeded one is `admin`) without being that admin. They would sign in as that admin.

With it off, a first sign-in under the name of an existing account is refused. The owner signs in with their password and links single sign-on in Settings > Users.

Signing out of Soulbeet does not sign you out of the provider. On a shared computer, sign out at the provider too, or the next person to click **Sign in with ...** gets your account.

### Upgrading to roles and folder roots

The release that adds admin roles changes a few things on the first start:

-   **Everyone is signed out once.** Sessions from earlier versions are no longer accepted. Sign in again.
-   **The seeded `admin` account is the only admin.** If it was deleted, the oldest account becomes admin instead. Every other account, Navidrome accounts included, becomes a plain user and no longer sees user management, the slskd settings, Security Checks or the folder administration. Sign in as `admin` and click **Make admin** next to your own account in Settings > Users. Before you upgrade, check that you can sign in as `admin`. If you no longer know the admin password and no other admin exists, nothing in the app gives the role back: stop Soulbeet, then run `sqlite3 soulbeet.db "UPDATE users SET role = 'admin' WHERE username = 'yourname'"` on the database file (in the `./data` volume of the Quick Start). The Soulbeet image has no `sqlite3`; if the host has none either, copy the file to a machine that has it while Soulbeet is stopped, run the command there and copy it back before starting Soulbeet.
-   **A generated secret key replaces the built-in one and any short one.** An install that ran without `SECRET_KEY` used a key built into Soulbeet, and earlier example files set short placeholders such as `a_secret_key` or `change-me-in-production`. Anyone who knows such a key can sign themselves in as any user, so Soulbeet no longer signs with the built-in key or with any `SECRET_KEY` under 32 bytes: on the first start it generates a key and stores it in `secret.key` next to the database. Everyone is signed out once. Stored Navidrome passwords are stored again under the new key on that start, so nobody has to enter theirs again. Back up `secret.key` with the database from now on, and remove a short `SECRET_KEY` (Security Checks lists it until you do). An install that sets a `SECRET_KEY` of at least 32 bytes keeps it.
-   **`NAVIDROME_MUSIC_PATH` names Soulbeet's own path now.** Earlier versions read it as the path Navidrome uses for its music. It now names where Soulbeet sees the root of Navidrome's default library. If you set it before, change it to the path inside the Soulbeet container (for example `/music/common` when Navidrome's library is `/media/music/common` and Soulbeet mounts `/media/music` at `/music`), or remove it. While it names no folder Soulbeet can see, the startup log and Settings > Library > Deletion Checks say so, and no song of that library is deleted or replaced.
-   **Behind a reverse proxy, the live Downloads panel needs the public name.** The download updates socket now opens only from Soulbeet's own page: the page's address must match the address the request was sent to, or `PUBLIC_URL`. A proxy must pass the public name in `Host` (nginx: `proxy_set_header Host $host;`) or `X-Forwarded-Host`, or `PUBLIC_URL` must be set to it. Otherwise the Downloads panel stays empty and the log says "Refused a download updates socket" with both addresses.
-   **Navidrome's ReportRealPath is no longer used.** Deletion and the rating sync find files from Navidrome's library paths. You can leave the player setting as it is; a "ReportRealPath not enabled" status is cleared on the first start.
-   **Usernames ignore letter case.** Accounts whose names differ only in letter case (`bob` and `Bob`) cannot sign in. The startup log lists them; an admin deletes the ones that are not used in Settings > Users.
-   **Signing out ends the session on the server**, so a copied cookie stops working too. Your other devices stay signed in.
-   **Registration is closed.** `REGISTRATION` now defaults to `navidrome` with `NAVIDROME_URL` set and to `admin` without it. Set `REGISTRATION=open` to keep the old behaviour.
-   **Folder roots.** The upgrade approves the parent directory of each existing admin folder, so an admin folder at `/music/main` approves `/music`. It never approves `/`, so a top-level admin folder such as `/music` approves nothing: add `/music` as a root and tick **Shared** on that folder before other users add folders inside it.
-   **Folder conflicts.** Existing folders that are outside every root, or that equal, contain or sit inside another user's folder, are listed under Folder Conflicts in Settings > Library. They keep working and can still be renamed in place, but a move to another path has to pass the folder rules. To solve one, tick **Shared** on one of the folders, or delete a folder entry there (its files stay on disk).

The release that adds the MCP server has its own upgrade notes in [docs/mcp.md](docs/mcp.md#upgrading).

### Beets Configuration

Soulbeet uses `beets` to import music. The image ships a config at `/app/beets_config.yaml`, with the `plugins:` line set for your image tier. Setting `BEETS_CONFIG` to another file **replaces** it, it does not merge with it. Copy the shipped file (see Quick Start) and edit the copy.

-   Which plugins each tier installs is listed in [docs/beets-plugins.md](./docs/beets-plugins.md). A plugin in `plugins:` that the tier does not install shows as a warning in Settings.
-   `plugins:` must contain `musicbrainz`. Without a metadata source plugin, Soulbeet marks beets as broken.
-   `include:` is ignored in the file you point `BEETS_CONFIG` at.
-   `mbtwopass` lives in `/opt/beets-plugins`. To keep it, set `pluginpath: [/opt/beets-plugins, /data/beets-plugins]`.
-   `discogs` needs `discogs.user_token`.
-   Soulbeet passes `--log`, which overrides `import.log`, and `--noincremental` and `--noresume`, which override `import.incremental` and `import.resume`.

Soulbeet runs every import through a small driver, `lib/soulbeet/src/beets/import.py`, with the Python that runs beets. The driver runs beets' own `beet` command line with the flags below and notes the matches beets weighed, so a skipped import can be finished with one of them (see [Skipped Imports](#skipped-imports)). It supports beets 2.8 to 2.11; with a beets it cannot hook into, Soulbeet marks beets as broken.

Default `beet import` flags used:
-   `-v`: Verbose output. The last lines of it (20 lines, 2 KiB) are shown under "beets output" on rows that carry a note: skipped, failed, or imported with a warning
-   `-c [config]`: The config file
-   `-q`: Quiet mode (no user interaction)
-   `--noincremental`: Import a folder again even if beets saw it before, so "Retry import" always runs
-   `--noresume`: Ignore beets' saved progress from an interrupted run
-   `-s`: Singleton mode (Default behavior unless `BEETS_ALBUM_MODE` is set)
-   `-l [library_path]`: Library database path (per-folder)
-   `-d [target_path]`: Import to the specific folder selected in the web UI.
-   `--log [file]`: Where beets writes its import log

#### Skipped Imports

In quiet mode beets skips files it cannot match closely, and the row says "No close match". Soulbeet keeps the downloaded files and the row offers:
-   **Retry import**: the same import again, for example after you changed the beets config.

When beets handled the import as one task (one track, or one album folder that beets reads as one album) and Soulbeet could read the matches beets weighed for it, the row also offers the two choices below. Otherwise it offers only Retry import.
-   **Show matches**: the matches beets weighed for that import, with their score and what differs. Pick one, or paste a MusicBrainz link or ID (a recording for singletons, a release in album mode), then click **Import with this match**. beets applies exactly that match (`-S <id>`), however low its score.
-   **Import as-is**: beets imports the files with their own tags (`-A`) and looks nothing up. Every file needs artist and title tags, and in album mode an album tag too. If one lacks them, nothing is imported and the row names the files that lack them: beets would file untagged music under empty names, and with `duplicate_action: remove` a second untagged import would replace the first.

### Image Tiers

Soulbeet ships three image tiers: `:light` (default, MusicBrainz tagging only), `:medium` (adds AcoustID fingerprint matching via the `chroma` plugin), and `:full` (adds cover art, lyrics, ReplayGain, and more). `:latest` is an alias for `:light` so existing installs keep their current behavior. See [docs/beets-plugins.md](./docs/beets-plugins.md) for what each tier ships, how to switch, and how to drop in your own plugins.

### Library Management

**Important**: Each music folder you configure in Soulbeet has its own beets database (`.beets_library.db`) stored at the root of that folder. This enables:

- Per-folder duplicate detection
- Independent library management for each user/folder
- Cross-library duplicate detection

#### Interacting with Your Library

Each user can have multiple libraries. Each library is a folder that contains music files and a `.beets_library.db` file. This database is used to avoid duplicate tracks within the same library.

Since we use different databases, we can't directly compare tracks across libraries. However, we can use the `beets` CLI to interact with each library individually. This way you can add tracks outside of Soulbeet but keep them in sync with your library.

To manually interact with a library (list tracks, modify tags, remove items, etc.), use the `beet` CLI with the `-l` flag pointing to the folder's database:

```bash
# List all tracks in a library
beet -l /music/Person1/.beets_library.db ls
```

If using Docker, run these commands inside the container:
```bash
docker exec -it <container_name> beet -l /music/Person1/.beets_library.db ls
```

For more beets commands, see the [beets documentation](https://beets.readthedocs.io/en/stable/reference/cli.html).

#### Album Mode (`BEETS_ALBUM_MODE`)

By setting `BEETS_ALBUM_MODE=true`, Soulbeet imports each download batch as an album instead of as singletons. Every batch is saved in its own folder named `<label> [<id>]` under the download path, and that folder is the album import.

This flag is only needed when you want album tags on your tracks. E.g: `albumartist` `mb_albumid` and so on..

**Important for Album Mode:**
Since `beets` is run in quiet mode (`-q`), it will skip any imports that require user intervention (e.g., if the metadata doesn't match confidently). A skipped album that beets read as one album can still be imported with a match you pick or as-is (see [Skipped Imports](#skipped-imports)). To ensure partial albums or less popular releases are imported correctly, you **must** configure your config file to be more permissive.

Recommended additions to your config file for Album Mode:

```yaml
import:
  # quiet_fallback: asis # Optional: If no strong match, import with existing tags instead of skipping
  timid: no

match:
  strong_rec_thresh: 0.10  # Lower threshold to accept less confident matches
  max_rec:
    missing_tracks: strong
    unmatched_tracks: strong
  distance_weights:
    missing_tracks: 0.1  # Reduce weight (I think default is 0.9) to penalize less for missing tracks, improving the overall score.
    # unmatched_tracks: 0.2  # Optional: Similar for extra tracks.
```

*Note: Tweaking `strong_rec_thresh` and other matching parameters increases the risk of incorrect tags, but is necessary for fully automated imports of obscure or partial albums.*

### Discovery Setup

Discovery generates personalized playlists from what you play and star in Navidrome and pushes them to Navidrome. Here's how to set it up.

#### Navidrome Configuration

Your Soulbeet folder paths must point to the same physical directories that Navidrome's music library uses. The mount paths inside each container can differ (e.g. Soulbeet at `/music`, Navidrome at `/media/music`), as long as they map to the same files on the host. Soulbeet learns where each Navidrome library is from the files it imports; set `NAVIDROME_MUSIC_PATH` to where Soulbeet sees Navidrome's default library to give it that root from the start.

#### Soulbeet Configuration

1. Go to **Settings > Library** in the Soulbeet web UI
2. Optionally add your **ListenBrainz username**. Soulbeet then reads your ListenBrainz history too. The token is optional: with one, ListenBrainz allows more requests and ranks an artist's top tracks itself
3. Optionally turn on **Also use Deezer for recommendations**. This sends the names of artists you play to Deezer, which has not approved this use, so it is off until you turn it on
4. **Enable Discovery** on a folder and pick your profiles (Conservative, Balanced, Adventurous)
5. Optionally customize playlist names for each profile
6. Hit **Generate** to run the first batch

Discovery starts from the songs you played, starred or rated 4 or 5 stars in Navidrome. Songs you rated 1 or 2 stars never count. With nothing played or starred yet, a run stops and the Discovery page says "No listening data yet. Play or star music in Navidrome." The page also lists the sources that fed the last run and any that failed.

#### How It Works

- Discovery creates a `Discovery/` directory inside your chosen folder, with subdirectories per profile (e.g., `Discovery/Balanced/`, `Discovery/Adventurous/`).
- Tracks are downloaded from Soulseek, imported via beets into the profile subdirectory, and tagged properly.
- A smart playlist is created in Navidrome (via the native API) for each profile, filtered by the folder path. The playlist auto-updates as Navidrome scans new files.
- Every 6 hours, an automation task syncs ratings, creates playlists if missing, and regenerates expired discovery batches. Recommendations for the profiles you selected are refreshed on their own 6-hour cycle, only for users with discovery on and a discovery folder chosen. The cycle starts at a random point in the first 6 hours after Soulbeet starts.

#### Auto-Delete

When enabled (Settings > Library > Auto-delete), 1-star tracks are deleted from disk during rating sync. Soulbeet finds each track's file from Navidrome's library path, so it needs the library root (learned from imports, or `NAVIDROME_MUSIC_PATH`). A track another listener rated above 1 star, starred or keeps in a playlist is kept, and so is any track Soulbeet cannot check; Settings > Library > Deletion Checks lists what was kept and why. [docs/mcp.md, "What every deletion checks"](docs/mcp.md#what-every-deletion-checks) lists every check.

## Development

1.  Install Rust and `dioxus_cli`.
2.  Run the tailwind watcher:
    ```bash
    ./css.sh
    ```
3.  Run the app:
    ```bash
    dx serve --platform web
    ```
4.  Run the tests (the Python tests need Python 3.11+ with beets and pytest installed):
    ```bash
    cargo test --workspace --features server,dioxus/server --lib
    cargo test -p soulbeet --lib
    python -m pytest -q lib/soulbeet/tests/probe lib/soulbeet/tests/import
    ```

### Refreshing the screenshots

The pictures in [screenshots](./screenshots) are taken on a running Soulbeet, so they show a real library. Nothing is downloaded or changed there: the downloads panel is staged, it shows the album you searched for part way through without fetching it.

```bash
cd e2e && npm ci && npx playwright install chromium
E2E_BASE_URL=https://music.example.com SHOTS_USER=you SHOTS_PASSWORD=... SHOTS_QUERY="an album title" npm run screenshots
```

## Roadmap

- Reduce friction: fewer clicks between "I want this" and "it's in my library"
- Play preview on album track list before downloading
- Smarter Soulseek search (better source ranking, automatic fallback between providers)
