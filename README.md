# Soulbeet

[![Docker Pulls](https://img.shields.io/docker/pulls/docccccc/soulbeet)](https://hub.docker.com/repository/docker/docccccc/soulbeet/general)
[![Docker Image Size](https://img.shields.io/docker/image-size/docccccc/soulbeet)](https://hub.docker.com/repository/docker/docccccc/soulbeet/general)
[![Docker Image Version](https://img.shields.io/docker/v/docccccc/soulbeet)](https://hub.docker.com/repository/docker/docccccc/soulbeet/general)

[![GitHub Actions Workflow Status](https://img.shields.io/github/actions/workflow/status/terry90/soulbeet/image-build-push.yml)](https://github.com/terry90/soulbeet/actions)
[![GitHub License](https://img.shields.io/github/license/terry90/soulbeet)](https://github.com/terry90/soulbeet)
[![GitHub Repo stars](https://img.shields.io/github/stars/terry90/soulbeet)](https://github.com/terry90/soulbeet)

Soulbeet is a modern, self-hosted music downloader and manager. It bridges the gap between Soulseek (via `slskd`) and your music library (managed by `beets`), providing a seamless flow from search to streaming-ready library.

Screenshots: [here](./screenshots)

## Features

-   **Unified Search**: Search for albums and tracks using MusicBrainz metadata and find sources on Soulseek.
-   **One-Click Download & Import**: Select an album (or just some tracks), choose your target folder, and Soulbeet handles the rest.
-   **Automated Importing**: Automatically monitors downloads and uses the `beets` CLI to tag, organize, and move files to your specified music folder.
-   **User Management**: Multi-user support with private folders. Each user can manage their own music library paths. Or have a common folder.

## Architecture

1.  **Soulbeet Web**: The main interface (Dioxus Fullstack).
2.  **Slskd**: The Soulseek client backend. Soulbeet communicates with `slskd` to initiate and monitor downloads.
3.  **Beets**: The music library manager. Soulbeet executes `beet import` to process finished downloads.
4.  **SQLite**: Stores user accounts and folder configurations. (PostgreSQL compat can be added easily, maybe in the future)

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
      - SLSKD_URL=http://slskd:5030
      - SLSKD_API_KEY=your_slskd_api_key_here
      # The path where slskd saves files (INSIDE the soulbeet container)
      - SLSKD_DOWNLOAD_PATH=/downloads
      # Optional: Beets configuration
      - BEETS_CONFIG=/config/config.yaml
      - SECRET_KEY=secret
    volumes:
      # Data persistence (DB)
      - ./data:/data
      # Map the SAME download folder slskd uses
      - /path/to/slskd/downloads:/downloads
      # Map your music libraries (where beets will move files to)
      - /path/to/music:/music
    # Optional
    depends_on:
      - slskd

  # Optional
  # Example slskd service if you don't have one running
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

2.  **Important**: The `/downloads` volume must match between `slskd` and `soulbeet` so Soulbeet can see the files `slskd` downloaded.

3.  Build and Run:

```bash
docker-compose up -d --build
```

### Initial Setup

1.  Open `http://localhost:9765`
2.  Login with the default credentials:
    -   Username: `admin`
    -   Password: `admin`
3.  Go to **Settings**.
4.  **Change your password** (Create a new user if you prefer and delete the admin later, or just change the admin logic if you forked the code).
5.  **Add Music Folders**: Add the paths where you want your music to be stored (e.g., `/music/Person1`, `/music/Person2`,  `/music/Shared`). These must be paths accessible inside the Docker container.

## Configuration

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `DATABASE_URL` | Connection string for SQLite | `sqlite:soulbeet.db` |
| `SLSKD_URL` | URL of your Slskd instance | |
| `SLSKD_API_KEY` | API Key for Slskd | |
| `SLSKD_DOWNLOAD_PATH` | Path where Slskd downloads files | |
| `BEETS_CONFIG` | Path to custom beets config file | `beets_config.yaml` |
| `BEETS_ALBUM_MODE` | Enable album import mode (see below) | `false` |
| `SECRET_KEY` | Used to encrypt tokens | |

### Beets Configuration

Soulbeet uses `beets` to import music. You can mount a custom `config.yaml` to `/config/config.yaml` (or wherever you point `BEETS_CONFIG` to) to customize how beets behaves (plugins, naming formats, etc.).

Default `beet import` flags used:
-   `-q`: Quiet mode (no user interaction)
-   `-s`: Singleton mode (Default behavior unless `BEETS_ALBUM_MODE` is set)
-   `-d [target_path]`: Import to the specific folder selected in the web UI.

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

## Development

1.  Install Rust and `dioxus_cli`.
2.  Run the tailwind watcher:
    ```bash
    ./css.sh
    ```
3.  Run the app:
    ```bash
    dx serve --platform web

## TODO & Ideas

- Mobile app (nothing much to do honestly)
- Better scoring
- Enhance the default beets configuration
- Find a way to avoid album dups ? e.g `Clair Obscur_ Expedition 33 (Original Soundtrack)` & `Clair Obscur_ Expedition 33_ Original Soundtrack` - Rare but annoying
- Add play preview on album track list
- Improve slskd search. Currently:
  - Single track search, query: "{artist} {track_title}" -> more resilient
  - Multiple tracks search, query: "{artist} {album}" -> best for metadata and grouping tracks by album
- Listenbrainz integration to autodownload suggestions
- Complete library manager, removal of tracks
- Synchronize a playlist (Spotify or other)
