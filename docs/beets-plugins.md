# Beets plugins in Soulbeet

Soulbeet uses [beets](https://beets.io/) to tag and organize your music after downloads finish. Beets has a [large plugin ecosystem](https://beets.readthedocs.io/en/stable/plugins/index.html) that extends what happens at import time: fingerprint matching, album-art fetching, lyrics, ReplayGain, and more.

This doc explains how to pick the right Soulbeet image for the plugins you want, and how to add plugins that don't ship in any tier.

## Image tiers

Soulbeet ships three image tiers. They are strictly additive: `full` is a superset of `medium`, which is a superset of `light`.

| Tier | Plugins enabled | Image size delta | Use when |
| --- | --- | --- | --- |
| light | `musicbrainz`, `mbtwopass` | baseline | You want the smallest image. Tagging via MusicBrainz only. |
| medium | light + `chroma` | +~40-60 MB | You want fingerprint matching (AcoustID) to catch files MusicBrainz alone can't match by length. |
| full | medium + `fetchart`, `embedart`, `lyrics`, `lastgenre`, `scrub`, `replaygain`[^rg], plus several zero-cost convenience plugins, and the tools of the YouTube Music download source[^yt] | +~150-200 MB on top of medium, plus ~120 MB (~47 MB compressed) for the YouTube Music tools | You want a complete tagging pipeline: cover art, lyrics, genres, normalization. |

[^yt]: yt-dlp, ytmusicapi and the deno JS runtime, in `/opt/ytdlp`, apart from beets. They do nothing until an admin turns YouTube Music on; see [ytdlp.md](ytdlp.md). The light and medium images do not have them.

[^rg]: The full tier wires `replaygain: { backend: ffmpeg, overwrite: yes }` in its baked `beets_config.yaml`. Override with care. See "Per-plugin configuration" below.

### Image tags

Each tier is published under three kinds of tag:

| Tier | Newest release | One release, fixed | Newest master build |
| --- | --- | --- | --- |
| light | `:latest` | `:v0.9.0` | `:light` |
| medium | `:latest-medium` | `:v0.9.0-medium` | `:medium` |
| full | `:latest-full` | `:v0.9.0-full` | `:full` |

- A release moves the `:latest` tags and `:light`, `:medium` and `:full` to its build, and adds its own version tags.
- Every commit to master moves `:light`, `:medium` and `:full` (and `:master` with its `-light`, `-medium` and `-full` variants), never the `:latest` tags. Between releases they hold code that is not released yet, so `:light` is not the same image as `:latest`.
- A test release such as `v1.2.3-rc.1` gets only its own tags, such as `:v1.2.3-rc.1`, `:v1.2.3-rc.1-medium` and `:v1.2.3-rc.1-full`, and moves none of the others.

To stay on releases, use a `:latest` tag or a version tag. Use `:light`, `:medium` or `:full` only if you want to run master.

### Picking a tier

- New install, unsure → start with light (`:latest`). You can switch later without losing data; only the image changes.
- Library has files where MusicBrainz refuses to match because the track length is off → medium. The `chroma` plugin uses AcoustID fingerprints, which bypass length checks.
- You want covers embedded, lyrics fetched, normalized loudness, and the kitchen sink → full.

Switch tiers by editing `image:` in your `docker-compose.yml` and recreating the container:

```yaml
services:
  soulbeet:
    image: docker.io/docccccc/soulbeet:latest-medium  # was :latest
```

```sh
docker compose pull soulbeet
docker compose up -d soulbeet
```

Your library, database, and config are unaffected.

### What ships in `:full`

Active by default in `:full`:

| Plugin | What it does |
| --- | --- |
| `chroma` | AcoustID fingerprint matching (also in `:medium`) |
| `musicbrainz` | MusicBrainz tag lookup (all tiers) |
| `mbtwopass` | Ships with Soulbeet, all tiers. Splits beets' single-track MusicBrainz search into two passes so short titles stop losing to repetitive ones. See below. |
| `fetchart` | Pulls album art from Cover Art Archive, iTunes, Amazon |
| `embedart` | Embeds the fetched art into the file tags |
| `lyrics` | Lyrics from LRCLIB, Genius, Google |
| `lastgenre` | Genres from Last.fm tags |
| `scrub` | Strips pre-existing tags before writing clean ones |
| `replaygain`[^rg] | Volume normalization (`ffmpeg` backend) |
| `mbsync` | Re-pulls MusicBrainz updates for already-imported items |
| `the` | Moves "The " articles to the end in path formats |
| `duplicates` | Finds duplicates by MBID |
| `missing` | Lists missing tracks per album |
| `importadded` | Preserves file mtimes as `added` date |
| `info` | Dumps tag values |
| `smartplaylist` | Auto-generates m3u files |
| `inline`, `types`, `ftintitle` | Path/format helpers |

Not a beets plugin, also in `:full` only: the YouTube Music download source's yt-dlp, ytmusicapi and deno (`/opt/ytdlp`), which `build/requirements-ytdlp.txt` pins by hash. The tier files set `YTDLP=true` for full only.

Available in `:full` but **not** enabled by default (need user config to be useful):

| Plugin | Why it's off | Enable by |
| --- | --- | --- |
| `discogs` | Needs your Discogs API token | Adding it to `plugins:` and setting `discogs.user_token:` in your mounted config |
| `web` | Optional admin UI (port 8337 by default) | Adding it to `plugins:` and exposing the port |
| `edit` | Needs `$EDITOR` and a TTY | Adding it to `plugins:` and running interactively |
| `lastimport` | Needs your Last.fm username | Adding it to `plugins:` and setting `lastfm.user:` |

## Tags with no value

beets keeps a field it has no value for as an empty text, an empty list or a 0, and writes it into the file as it is: an empty tag, or `0000` as the original date when the match gave none. Soulbeet leaves those tags out of every file beets writes during an import, on every tier and with any `BEETS_CONFIG`, the writes of `scrub`, `embedart`, `lyrics`, `lastgenre`, `replaygain` and `ftintitle` included. An empty or zero tag the file already had for such a field is removed.

- Text and lists: an empty value is left out.
- Numbers: 0 is left out where beets cannot tell it from no value, which covers track and disc numbers and their totals, `bpm` and the parts of a date. A year of 0 leaves the whole date out. A year without month and day is written as the year alone.
- A ReplayGain or R128 gain of 0.0 is a value and is kept, and so is a compilation flag of false.
- On M4A, a track or disc number pair that holds two zeros is left out.
- A field a plugin adds without a type is written as beets writes it.

This is part of Soulbeet's import driver, not a beets plugin, so it is not in `plugins:` and needs no configuration. A `beet` command you run yourself in the container (`beet write`, `beet modify`, `beet mbsync`) writes tags as beets does, empty ones included. Files imported by earlier versions keep the tags they got then.

## Adding a plugin that's not in your tier

### Option 1: Switch to a fatter tier (preferred)

If the plugin is in full but you're on light, switch to `:latest-full`. See above.

### Option 2: Drop a single Python file (pure-Python plugins only)

For one-off plugins that are a single `.py` file with no extra pip dependencies, for example a custom `BeetsPlugin` you wrote yourself, or one of the [single-file community plugins](https://beets.readthedocs.io/en/stable/plugins/index.html#other-plugins), you can drop them in without rebuilding the image.

Soulbeet's default config sets `pluginpath: [/opt/beets-plugins, /data/beets-plugins]`, and the runtime image pre-creates `/data/beets-plugins/`. Mount your plugin directory there and list the plugin in `plugins:`.

1. Put your plugin file on the host. Example:
   ```
   ./my-beets-plugins/
   └── hello.py
   ```
   The file must subclass `beets.plugins.BeetsPlugin`. **No `__init__.py` is needed**: `pluginpath` extends beets' PEP 420 namespace package.

2. Mount it into the container in `docker-compose.yml`:
   ```yaml
   volumes:
     - ./my-beets-plugins:/data/beets-plugins:ro
   ```

3. Add the plugin name to the `plugins:` line in your config file:
   ```yaml
   plugins: chroma musicbrainz fetchart hello
   ```

4. Restart Soulbeet:
   ```sh
   docker compose up -d soulbeet
   ```

5. Check logs (`docker compose logs soulbeet`) for import errors. If the plugin uses a Python module that isn't already installed in the image, you'll see `ModuleNotFoundError` here and need Option 3.

### Option 3: Build your own image FROM a Soulbeet tier

For plugins that need extra pip packages (e.g. [beetcamp](https://pypi.org/project/beetcamp/), [beets-spotify](https://pypi.org/project/beets-spotify/)), build a thin image on top of a Soulbeet tier. Soulbeet's runtime is distroless (no shell, no pip at runtime), so you need a two-stage build that adds your packages into a copy of the venv.

```dockerfile
# my-soulbeet/Dockerfile
FROM python:3.11-slim-bookworm AS plugin-builder
COPY --from=docker.io/docccccc/soulbeet:v0.9.0-medium /opt/venv /opt/venv
ENV PATH="/opt/venv/bin:$PATH"
RUN pip install --no-cache-dir beetcamp beets-spotify

FROM docker.io/docccccc/soulbeet:v0.9.0-medium
COPY --from=plugin-builder /opt/venv /opt/venv
```

Build and use:

```sh
docker build -t my-soulbeet:latest ./my-soulbeet
```

```yaml
services:
  soulbeet:
    image: my-soulbeet:latest  # was docker.io/docccccc/soulbeet:latest-medium
```

Then enable the new plugin in your config file as usual.

Both `FROM` lines name the same version tag. You'll rebuild this image each time you bump it to a newer Soulbeet version.

## What won't work

The runtime image is distroless: no shell, no apt, no pip, and a read-only filesystem except for `/data` and any mounts you add. That rules out:

- **Plugins requiring native libraries the image doesn't ship.** Examples: `keyfinder` (external binary, no Debian package), `bpd` (GStreamer userspace), `convert` on `:light` or `:medium` (needs `ffmpeg`, only in `:full`). Adding these via Option 2 won't help, the missing binary still isn't there.
- **Plugins requiring extra pip packages, via Option 2.** Use Option 3 (custom image FROM tier) instead.
- **Plugins writing outside `/data`.** Anything assuming `~/.cache/`, `/tmp/foo`, or arbitrary paths will hit a read-only filesystem error. The `thumbnails` plugin is a known example.
- **Python version mismatch.** The venv is pinned to Python 3.11. Wheel-only plugins built against a different cpython ABI won't load.

## Per-plugin configuration

All beets plugin config goes in your config file. `BEETS_CONFIG` replaces the shipped `/app/beets_config.yaml`, so start from a copy of it. Point `BEETS_CONFIG` at your file:

```yaml
services:
  soulbeet:
    environment:
      - BEETS_CONFIG=/config/config.yaml
    volumes:
      - ./beets:/config:ro
```

Sensitive values (Discogs token, Last.fm username, Genius API key, AcoustID submit key) live in that YAML. `chmod 600` on the host file is a reasonable baseline. The default AcoustID lookup key shipped with beets is sufficient for `chroma` to work; you only need your own AcoustID key if you want to submit fingerprints back.

- **`mbtwopass` (all tiers, ships with the image):** No configuration. It changes how single tracks are looked up in MusicBrainz. Beets sends `alias: <title>` alongside `recording: <title>`, which does not narrow the search but adds tokens to it, and MusicBrainz ranks a recording higher the more often it repeats them. A short title loses to a long repetitive one: searching Kowloon's "Wake Up" returns "Up Up Up Up Up Up" at 37.9% ahead of the exact recording, far enough off that a quiet import skips the file without saying why. The plugin searches without the alias term first and only retries with it when nothing came back close enough to import, so the alternate-title case it exists for still works. Measured over 25 real downloads, the alias term helped 4 files and hurt 1; running both passes takes the better of the two every time. The plugin lives at `/opt/beets-plugins`, not `/data/beets-plugins`, because deployments bind-mount `/data` and would hide it. If you replace `pluginpath`, keep `/opt/beets-plugins` in the list.
- **`replaygain` (full tier only):** Soulbeet's `full` tier ships `replaygain: { backend: ffmpeg, overwrite: yes }` baked into the default `beets_config.yaml`. The distroless runtime does not include GStreamer (which the replaygain plugin would otherwise use as a default backend), so the ffmpeg backend is required. If you set your own `BEETS_CONFIG`, preserve this block or `beet replaygain` will fail to load with `No module named 'gi'`.

## Reference

- [Beets plugin index](https://beets.readthedocs.io/en/stable/plugins/index.html)
- [Plugin configuration (`plugins:` + `pluginpath:`)](https://beets.readthedocs.io/en/stable/reference/config.html#plugins)
- [Writing a plugin](https://beets.readthedocs.io/en/stable/dev/plugins.html)
- [chroma + AcoustID](https://beets.readthedocs.io/en/stable/plugins/chroma.html)
- [Advanced Awesomeness (canonical power-user config)](https://docs.beets.io/en/stable/guides/advanced.html)
