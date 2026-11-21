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

Soulbeet is an MCP server, so any AI assistant that speaks MCP (Claude, ChatGPT, Cursor, your own agent...) can drive it. You ask for music in plain words, for example "Get In Rainbows by Radiohead into my library", "How is that batch of downloads going?" or "Do I have duplicates in my library?". The assistant checks your library, searches Soulseek, downloads, imports and builds the Navidrome playlist, and you never open the web UI. Downloads keep running after you close the chat.

Serve Soulbeet over https at the root of a name (public, for hosted assistants such as claude.ai), then add `https://music.example.com/mcp` as a remote MCP server in your client and click **Allow** when Soulbeet asks. Clients sign in with OAuth, so there is no API key to copy around. Each person connects their own account, and the assistant acts as that person only. It cannot delete music until you allow it in Settings > Users > Connected apps.

[docs/mcp.md](docs/mcp.md) has the setup steps, the proxy examples (Caddy, nginx, Traefik, Cloudflare tunnel), every tool, the limits and the error codes.

## Getting Started with Docker

The recommended way to run Soulbeet is via Docker Compose. This ensures all dependencies (like `beets` and `python`) are correctly set up. The image supports both **AMD64** and **ARM64**.

### Prerequisites

-   Docker & Docker Compose (or podman-compose)
-   **slskd 0.26 or newer** (downloads are enqueued through the batches API introduced in 0.26). The compose file below runs one.

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

2.  `SECRET_KEY` signs sessions and encrypts stored Navidrome passwords. It is optional: without it, Soulbeet generates a key on the first start and stores it in `secret.key` next to the database (in the `./data` volume above). Back up that file together with the database: losing or changing the key signs everyone out and makes the stored Navidrome passwords unreadable until each user enters theirs again. To set your own key instead, put a random value of at least 32 bytes in a `.env` file next to `docker-compose.yml` (a shorter key is not used, see [configuration](docs/configuration.md)). Skip this if your `.env` already sets `SECRET_KEY`; the command adds a line and keeps whatever the file already holds:

```bash
echo "SECRET_KEY=$(openssl rand -hex 32)" >> .env
```

3.  Seed the beets config. `BEETS_CONFIG` replaces the config shipped in the image, so start from a copy of it. Use the same tag as the `image:` line above, since the shipped `plugins:` line depends on the tier (see [Image tiers](docs/beets.md#image-tiers)), and copy it again after you change tier:

```bash
TAG=latest # the tag on your image: line (latest, light, medium or full)
mkdir -p beets
docker create --name sb "docker.io/docccccc/soulbeet:$TAG"
docker cp sb:/app/beets_config.yaml ./beets/config.yaml
docker rm sb
```

4.  **Important**: The `/downloads` volume must match between `slskd` and `soulbeet` so Soulbeet can see the files `slskd` downloaded. The `/music` volume must match between `soulbeet` and `navidrome` so Navidrome can see the organized library.

5.  Run:

```bash
docker-compose up -d
```

### Initial Setup

A fresh install has one account, `admin` with the password `admin`. It is the only admin. That password only signs in to choose a new one: until it is changed, the account can do nothing else. Everyone sees the Settings tabs, but only admins see user management in Settings > Users, the slskd settings and Security Checks in Settings > Config, and the folder administration in Settings > Library.

1.  Open `http://localhost:9765` and sign in as `admin` / `admin`, with or without Navidrome. Soulbeet asks for a new password before anything else; enter `admin` as the current password and choose yours.
2.  In **Settings > Config**, connect slskd (URL + API key). [How to get an slskd API key](https://github.com/slskd/slskd/blob/master/docs/config.md#yaml-24).
3.  In **Settings > Library**, add your music folders (e.g. `/music`). Admins can add a folder anywhere.
4.  Let the other users in:
    - **With Navidrome** (`NAVIDROME_URL` set): everyone signs in with their **Navidrome credentials**, and their Soulbeet account is created on the first sign-in. To make your own Navidrome account an admin, sign in with it once, then sign in as `admin` and click **Make admin** next to it in Settings > Users.
    - **Without Navidrome**: create the accounts in Settings > Users.
    - **With a login provider** such as Pocket ID or Authelia: see [single sign-on](docs/sso.md).
5.  Users who are not admins can only add folders inside a folder root an admin approved. Add one under **Approved Folder Roots** in Settings > Library, for example `/music`, so a user can add `/music/alice`. The root itself cannot be the folder of a user who is not an admin. If one of your own folders is that root (an admin folder at `/music`), tick **Shared** next to it, or every folder inside it is refused as being inside another user's folder.
6.  That's it. Search for something and download it.

For discovery (optional): enable discovery on a folder in Settings > Library, pick your profiles, and hit Generate. Play or star some music in Navidrome first, since that is what discovery starts from. You can also add your ListenBrainz username there. [docs/discovery.md](docs/discovery.md) has the details.

**Upgrading** from a version without admin roles? Read [docs/upgrading.md](docs/upgrading.md) first: it signs everyone out and makes the seeded `admin` the only admin.

## Guides

- [Configuration](docs/configuration.md): the environment variables and their defaults (the `OIDC_*` ones are in Single sign-on), how sign-in works with Navidrome, and which settings live in the web UI.
- [Single sign-on](docs/sso.md): signing in through an OpenID Connect provider (Pocket ID, Authelia, Authentik, Keycloak), how accounts are matched, and when to turn off linking by name.
- [Upgrading](docs/upgrading.md): what changes on the first start after the release that adds admin roles and folder roots.
- [Health check](docs/health-check.md): the `/healthz` address for Docker and Kubernetes health checks, with a compose `healthcheck` and Kubernetes probes.
- [Beets](docs/beets.md): the beets config file, import flags, skipped imports, image tiers, per-folder libraries and album mode.
- [Beets plugins](docs/beets-plugins.md): the plugins each image tier ships, switching tiers, and adding your own plugins.
- [Discovery](docs/discovery.md): setting up discovery, how its playlists and 6-hour cycle work, and auto-delete.
- [MCP server](docs/mcp.md): connecting an AI assistant, reverse proxy examples, every tool, the deletion checks and the error codes.
- [Development](docs/development.md): running Soulbeet from source, the tests, and refreshing the screenshots.

## Development

To run Soulbeet from source, run the tests or refresh the screenshots, see [docs/development.md](docs/development.md).

## Roadmap

- Reduce friction: fewer clicks between "I want this" and "it's in my library"
- Play preview on album track list before downloading
- Smarter Soulseek search (better source ranking, automatic fallback between providers)
