# Soulbeet

[![Docker Pulls](https://img.shields.io/docker/pulls/docccccc/soulbeet)](https://hub.docker.com/r/docccccc/soulbeet)
[![Docker Image Size](https://img.shields.io/docker/image-size/docccccc/soulbeet)](https://hub.docker.com/r/docccccc/soulbeet/tags)
[![Docker Image Version](https://img.shields.io/docker/v/docccccc/soulbeet)](https://hub.docker.com/r/docccccc/soulbeet/tags)

[![GitHub Actions Workflow Status](https://img.shields.io/github/actions/workflow/status/terry90/soulbeet/image-build-push.yml)](https://github.com/terry90/soulbeet/actions)
[![GitHub License](https://img.shields.io/github/license/terry90/soulbeet)](https://github.com/terry90/soulbeet)
[![GitHub Repo stars](https://img.shields.io/github/stars/terry90/soulbeet)](https://github.com/terry90/soulbeet)

Soulbeet is a self-hosted music downloader, library manager, and discovery engine. Search for music, download it from Soulseek, auto-tag with beets, and get weekly discovery playlists pushed to your Navidrome server based on your Last.fm and ListenBrainz listening history.

Screenshots: [here](./screenshots)

## Features

- **Search & Download**: Find albums and tracks via MusicBrainz or Last.fm, then download from Soulseek in one click. Beets handles tagging and organization.
- **Music Discovery**: Get personalized recommendations based on your scrobble history. Soulbeet analyzes your listening across Last.fm and ListenBrainz, finds new music through track similarity, artist exploration, collaborative filtering, and genre discovery, then downloads the best candidates and creates Navidrome playlists for you.
- **Three Discovery Profiles**: Conservative (stay close to what you know), Balanced, or Adventurous (push into unfamiliar territory). Run one or all three in parallel, each with its own playlist.
- **Rate & Keep**: Listen to discovery tracks in Navidrome. Rate them: 3+ stars promotes to your library, 1 star deletes it. Unrated tracks expire after a configurable lifetime and get replaced with a fresh batch.
- **Multi-user**: Private or shared folders for families and friend groups. Each user has their own discovery profiles, scrobble credentials, and preferences. Shared folders respect everyone's ratings before auto-deleting.
- **Multiple Metadata Providers**: MusicBrainz (better for albums) or Last.fm (better for single tracks), selectable per user.

## How It Works

1. **Soulbeet Web** -- the main interface (Dioxus fullstack app)
2. **Slskd** -- Soulseek P2P client, handles the actual downloads
3. **Beets** -- tags, organizes, and moves files into your library
4. **Navidrome** -- streams your music, hosts discovery playlists, provides rating feedback
5. **Last.fm / ListenBrainz** -- scrobble services that feed the recommendation engine
6. **SQLite** -- stores everything (users, folders, candidates, engine reports, profiles)

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
      - SECRET_KEY=change-me-in-production
      - NAVIDROME_URL=http://navidrome:4533
      # Optional
      - BEETS_CONFIG=/config/config.yaml
    volumes:
      - ./data:/data
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

2.  **Important**: The `/downloads` volume must match between `slskd` and `soulbeet` so Soulbeet can see the files `slskd` downloaded. The `/music` volume must match between `soulbeet` and `navidrome` so Navidrome can see the organized library.

3.  Run:

```bash
docker-compose up -d
```

### Initial Setup

1.  Open `http://localhost:9765`
2.  Log in -- Soulbeet authenticates against your Navidrome server, so use your Navidrome credentials.
3.  Go to **Settings**.
4.  **Configure slskd connection** (Settings > Config): Add your slskd URL (e.g., `http://slskd:5030`) and API key. Get your API key from slskd config file or [add one](https://github.com/slskd/slskd/blob/master/docs/config.md#yaml-24).
5.  **Add Music Folders** (Settings > Library): Add the paths where you want your music stored (e.g., `/music/Person1`, `/music/Person2`, `/music/Shared`). These must be paths accessible inside the Docker container.
6.  **Set up Discovery** (Settings > Library): Add your Last.fm API key and/or ListenBrainz token. Enable discovery on a folder, pick your profiles, and hit Generate.
7.  **Enable scrobbling in Navidrome**: Go to your Navidrome personal settings and enable Last.fm/ListenBrainz scrobbling. The more listening history these services have, the better the recommendations get.

## Configuration

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `DATABASE_URL` | Connection string for SQLite | `sqlite:soulbeet.db` |
| `DOWNLOAD_PATH` | Path where slskd saves downloads | `/downloads` |
| `SECRET_KEY` | Encryption key for tokens and credentials | |
| `NAVIDROME_URL` | Your Navidrome server URL | |
| `BEETS_CONFIG` | Path to custom beets config file | `beets_config.yaml` |
| `BEETS_ALBUM_MODE` | Enable album import mode (see below) | `false` |

**Note**: slskd URL and API key are configured through the web UI (Settings > Config) and stored in the database. Scrobble credentials (Last.fm API key, ListenBrainz token) are configured per-user in Settings > Library.

### Beets Configuration

Soulbeet uses `beets` to import music. You can mount a custom `config.yaml` to `/config/config.yaml` (or wherever you point `BEETS_CONFIG` to) to customize how beets behaves (plugins, naming formats, etc.).

Default `beet import` flags used:
-   `-q`: Quiet mode (no user interaction)
-   `-s`: Singleton mode (Default behavior unless `BEETS_ALBUM_MODE` is set)
-   `-l [library_path]`: Library database path (per-folder)
-   `-d [target_path]`: Import to the specific folder selected in the web UI.

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

By setting `BEETS_ALBUM_MODE=true`, Soulbeet will attempt to group downloaded files by their parent directory and import them as an album instead of singletons.

This flag is only needed when you want album tags on your tracks. E.g: `albumartist` `mb_albumid` and so on..

**Important for Album Mode:**
Since `beets` is run in quiet mode (`-q`), it will skip any imports that require user intervention (e.g., if the metadata doesn't match confidently). To ensure partial albums or less popular releases are imported correctly, you **must** configure your `beets_config.yaml` to be more permissive.

Recommended additions to your `beets_config.yaml` for Album Mode:

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

Discovery generates personalized playlists from your scrobble history and pushes them to Navidrome. Here's how to set it up.

#### Navidrome Configuration

1. **Enable ReportRealPath** for the Soulbeet player. Go to your Navidrome instance > Settings > Players (e.g. `https://your-navidrome/app/#/player`), find the Soulbeet player entry, and enable "Report Real Path". Without this, rating sync and auto-delete cannot resolve file paths.

2. Your Soulbeet folder paths must point to the same physical directories that Navidrome's music library uses. The mount paths inside each container can differ (e.g. Soulbeet at `/music`, Navidrome at `/media/music`), as long as they map to the same files on the host.

#### Soulbeet Configuration

1. Go to **Settings > Library** in the Soulbeet web UI
2. Add your **Last.fm API key** and/or **ListenBrainz username + token**
3. **Enable Discovery** on a folder and pick your profiles (Conservative, Balanced, Adventurous)
4. Optionally customize playlist names for each profile
5. Hit **Generate** to run the first batch

#### How It Works

- Discovery creates a `Discovery/` directory inside your chosen folder, with subdirectories per profile (e.g., `Discovery/Balanced/`, `Discovery/Adventurous/`).
- Tracks are downloaded from Soulseek, imported via beets into the profile subdirectory, and tagged properly.
- A smart playlist is created in Navidrome (via the native API) for each profile, filtered by the folder path. The playlist auto-updates as Navidrome scans new files.
- Every 6 hours, an automation task syncs ratings, creates playlists if missing, and regenerates expired discovery batches.

#### Auto-Delete

When enabled (Settings > Library > Auto-delete), 1-star tracks are deleted from disk during rating sync. This requires ReportRealPath to be enabled in Navidrome so Soulbeet receives the actual file path. For shared folders (multiple users), a track is only deleted if the average rating across all users is 1 or below.

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

## TODO & Ideas

- Mobile app (nothing much to do honestly)
- Enhance the default beets configuration
- Find a way to avoid album dups ? e.g `Clair Obscur_ Expedition 33 (Original Soundtrack)` & `Clair Obscur_ Expedition 33_ Original Soundtrack` - Rare but annoying
- Add play preview on album track list
- Improve slskd search. Currently:
  - Single track search, query: "{artist} {track_title}" -> more resilient
  - Multiple tracks search, query: "{artist} {album}" -> best for metadata and grouping tracks by album
- Synchronize a playlist (Spotify or other)
