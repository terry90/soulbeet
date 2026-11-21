# Beets

This page covers how Soulbeet runs beets: the config file, the import flags, skipped imports, image tiers, per-folder libraries and album mode. The plugins each image tier ships are in [beets-plugins.md](beets-plugins.md).

## Beets configuration

Soulbeet uses `beets` to import music. The image ships a config at `/app/beets_config.yaml`, with the `plugins:` line set for your image tier. Setting `BEETS_CONFIG` to another file **replaces** it, it does not merge with it. Copy the shipped file (see [Quick Start](../README.md#quick-start)) and edit the copy.

-   Which plugins each tier installs is listed in [beets-plugins.md](beets-plugins.md). A plugin in `plugins:` that the tier does not install shows as a warning in Settings.
-   `plugins:` must contain `musicbrainz`. Without a metadata source plugin, Soulbeet marks beets as broken.
-   `include:` is ignored in the file you point `BEETS_CONFIG` at.
-   `mbtwopass` lives in `/opt/beets-plugins`. To keep it, set `pluginpath: [/opt/beets-plugins, /data/beets-plugins]`.
-   `discogs` needs `discogs.user_token`.
-   Soulbeet passes `--log`, which overrides `import.log`, and `--noincremental` and `--noresume`, which override `import.incremental` and `import.resume`.

Soulbeet runs every import through a small driver, `lib/soulbeet/src/beets/import.py`, with the Python that runs beets. The driver runs beets' own `beet` command line with the flags below and notes the matches beets weighed, so a skipped import can be finished with one of them (see [Skipped imports](#skipped-imports)). It supports beets 2.8 to 2.11; with a beets it cannot hook into, Soulbeet marks beets as broken.

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

## Skipped imports

In quiet mode beets skips files it cannot match closely, and the row says "No close match". Soulbeet keeps the downloaded files and the row offers:
-   **Retry import**: the same import again, for example after you changed the beets config.

When beets handled the import as one task (one track, or one album folder that beets reads as one album) and Soulbeet could read the matches beets weighed for it, the row also offers the two choices below. Otherwise it offers only Retry import.
-   **Show matches**: the matches beets weighed for that import, with their score and what differs. Pick one, or paste a MusicBrainz link or ID (a recording for singletons, a release in album mode), then click **Import with this match**. beets applies exactly that match (`-S <id>`), however low its score.
-   **Import as-is**: beets imports the files with their own tags (`-A`) and looks nothing up. Every file needs artist and title tags, and in album mode an album tag too. If one lacks them, nothing is imported and the row names the files that lack them: beets would file untagged music under empty names, and with `duplicate_action: remove` a second untagged import would replace the first.

## Image tiers

Soulbeet ships three image tiers: light (the default, MusicBrainz tagging only), medium (adds AcoustID fingerprint matching via the `chroma` plugin), and full (adds cover art, lyrics, ReplayGain, and more). `:latest`, `:latest-medium` and `:latest-full` are the newest release of each tier. `:light`, `:medium` and `:full` follow master instead: every commit to master moves them, so between releases they hold code that is not released yet, and `:light` is then a newer image than `:latest`. See [beets-plugins.md](beets-plugins.md#image-tags) for every tag, and for what each tier ships, how to switch, and how to drop in your own plugins.

## Library management

**Important**: Each music folder you configure in Soulbeet has its own beets database (`.beets_library.db`) stored at the root of that folder. This enables:

- Per-folder duplicate detection
- Independent library management for each user/folder
- Cross-library duplicate detection

### Interacting with your library

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

## Album mode (`BEETS_ALBUM_MODE`)

By setting `BEETS_ALBUM_MODE=true`, Soulbeet imports each download batch as an album instead of as singletons. Every batch is saved in its own folder named `<label> [<id>]` under the download path, and that folder is the album import.

This flag is only needed when you want album tags on your tracks. E.g: `albumartist` `mb_albumid` and so on..

**Important for Album Mode:**
Since `beets` is run in quiet mode (`-q`), it will skip any imports that require user intervention (e.g., if the metadata doesn't match confidently). A skipped album that beets read as one album can still be imported with a match you pick or as-is (see [Skipped imports](#skipped-imports)). To ensure partial albums or less popular releases are imported correctly, you **must** configure your config file to be more permissive.

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
