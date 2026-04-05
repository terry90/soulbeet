# Soulbeet

[![Docker Pulls](https://img.shields.io/docker/pulls/docccccc/soulbeet)](https://hub.docker.com/r/docccccc/soulbeet)
[![Docker Image Size](https://img.shields.io/docker/image-size/docccccc/soulbeet)](https://hub.docker.com/r/docccccc/soulbeet/tags)
[![Docker Image Version](https://img.shields.io/docker/v/docccccc/soulbeet)](https://hub.docker.com/r/docccccc/soulbeet/tags)

[![GitHub Actions Workflow Status](https://img.shields.io/github/actions/workflow/status/terry90/soulbeet/image-build-push.yml)](https://github.com/terry90/soulbeet/actions)
[![GitHub License](https://img.shields.io/github/license/terry90/soulbeet)](https://github.com/terry90/soulbeet)
[![GitHub Repo stars](https://img.shields.io/github/stars/terry90/soulbeet)](https://github.com/terry90/soulbeet)

Soulbeet is a self-hosted music downloader, library manager, and discovery engine. Search for music, download it, and let the app handle everything else: finding the best source, tagging, organizing, and keeping your library clean. Connect your scrobble history and Soulbeet will find new music for you automatically.

Screenshots: [here](./screenshots)

## Features

- **Search & Download**: Find albums and tracks, hit download. Soulbeet picks the best available source from Soulseek, downloads it, tags it with beets, and puts it in your library. No manual file management.
- **Music Discovery**: Soulbeet analyzes your Last.fm and ListenBrainz history, finds new music through track similarity, artist exploration, collaborative filtering, and genre discovery, downloads the best candidates, and pushes playlists to your Navidrome server. Fully automatic.
- **Three Discovery Profiles**: Conservative (close to what you know), Balanced, or Adventurous (unfamiliar territory). Run one or all three, each with its own playlist.
- **Rate & Keep**: Listen in Navidrome. 3+ stars promotes a track to your permanent library, 1 star deletes it. Unrated tracks expire and get replaced with fresh picks.
- **Multi-user**: Private or shared folders. Each user gets their own discovery profiles, scrobble credentials, and preferences.
- **Multiple Metadata Providers**: MusicBrainz (albums) or Last.fm (single tracks), selectable per user.

## How It Works

You search, you click download. Behind the scenes:

1. **Soulbeet** finds the best source on Soulseek, downloads it through **slskd**, tags and organizes it with **beets**, and puts it in your library.
2. **Navidrome** picks up the new files and makes them streamable.
3. For discovery: **Last.fm / ListenBrainz** feed your listening history into the recommendation engine, which finds new tracks, downloads them, and pushes playlists to Navidrome.

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

1.  Open `http://localhost:9765` and log in with your **Navidrome credentials**.
2.  In **Settings > Config**, connect slskd (URL + API key). [How to get an slskd API key](https://github.com/slskd/slskd/blob/master/docs/config.md#yaml-24).
3.  In **Settings > Library**, add your music folders (e.g. `/music`).
4.  That's it. Search for something and download it.

For discovery (optional): add your Last.fm API key and/or ListenBrainz token in Settings > Library, enable discovery on a folder, pick your profiles, and hit Generate.

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

## Roadmap

- Reduce friction: fewer clicks between "I want this" and "it's in my library"
- Play preview on album track list before downloading
- Smarter Soulseek search (better source ranking, automatic fallback between providers)
