# Beets

This page covers how Soulbeet runs beets: the config file, the import flags, skipped imports, image tiers, library databases and album mode. The plugins each image tier ships are in [beets-plugins.md](beets-plugins.md).

## Beets configuration

Soulbeet uses `beets` to import music. The image ships a config at `/app/beets_config.yaml`, with the `plugins:` line set for your image tier. Setting `BEETS_CONFIG` to another file **replaces** it, it does not merge with it. Copy the shipped file (see [Quick Start](../README.md#quick-start)) and edit the copy.

-   Which plugins each tier installs is listed in [beets-plugins.md](beets-plugins.md). A plugin in `plugins:` that the tier does not install shows as a warning in Settings.
-   `plugins:` must contain `musicbrainz`. Without a metadata source plugin, Soulbeet marks beets as broken.
-   `include:` is ignored in the file you point `BEETS_CONFIG` at.
-   `mbtwopass` lives in `/opt/beets-plugins`. To keep it, set `pluginpath: [/opt/beets-plugins, /data/beets-plugins]`.
-   `discogs` needs `discogs.user_token`.
-   Soulbeet passes `--log`, which overrides `import.log`, and `--noincremental` and `--noresume`, which override `import.incremental` and `import.resume`.
-   `library:` is not used: Soulbeet passes each music folder's own database with `-l` (see [Library databases](#library-databases)).

Soulbeet runs every import through a small driver, `lib/soulbeet/src/beets/import.py`, with the Python that runs beets. The driver runs beets' own `beet` command line with the flags below and notes the matches beets weighed, so a skipped import can be finished with one of them (see [Skipped imports](#skipped-imports)). It supports beets 2.8 to 2.11; with a beets it cannot hook into, Soulbeet marks beets as broken.

Default `beet import` flags used:
-   `-v`: Verbose output. The last lines of it (20 lines, 2 KiB) are shown under "beets output" on rows that carry a note: skipped, failed, or imported with a warning
-   `-c [config]`: The config file
-   `-q`: Quiet mode (no user interaction)
-   `--noincremental`: Import a folder again even if beets saw it before, so "Retry import" always runs
-   `--noresume`: Ignore beets' saved progress from an interrupted run
-   `-s`: Singleton mode (Default behavior unless `BEETS_ALBUM_MODE` is set)
-   `-l [library_path]`: The music folder's library database (see [Library databases](#library-databases))
-   `-d [target_path]`: Import to the specific folder selected in the web UI.
-   `--log [file]`: Where beets writes its import log

## Skipped imports

In quiet mode beets skips files it cannot match closely, and the row says "No close match". Soulbeet keeps the downloaded files and the row offers:
-   **Retry import**: the same import again, for example after you changed the beets config.

When beets handled the import as one task (one track, or one album folder that beets reads as one album) and Soulbeet could read the matches beets weighed for it, the row also offers the two choices below. Otherwise it offers only Retry import.
-   **Show matches**: the matches beets weighed for that import, with their score and what differs. Pick one, or paste a MusicBrainz link or ID (a recording for singletons, a release in album mode), then click **Import with this match**. beets applies exactly that match (`-S <id>`), however low its score.
-   **Import as-is**: beets imports the files with their own tags (`-A`) and looks nothing up. Every file needs artist and title tags, and in album mode an album tag too. If one lacks them, nothing is imported and the row names the files that lack them: beets would file untagged music under empty names, and with `duplicate_action: remove` a second untagged import would replace the first.

## Image tiers

Soulbeet ships three image tiers: light (the default, MusicBrainz tagging only), medium (adds AcoustID fingerprint matching via the `chroma` plugin), and full (adds cover art, lyrics, ReplayGain, and more). `:latest`, `:latest-medium` and `:latest-full` are the newest release of each tier. `:light`, `:medium` and `:full` follow master instead: every commit to master and every release moves them, so between releases they hold code that is not released yet, and `:light` is then a newer image than `:latest`. See [beets-plugins.md](beets-plugins.md#image-tags) for every tag, and for what each tier ships, how to switch, and how to drop in your own plugins.

## Library databases

Each music folder you configure in Soulbeet has its own beets library database. beets uses it to find duplicates within that folder, and Soulbeet uses it to forget or move a track of that folder when it is deleted or replaced. One beets database cannot hold several music folders, which is why the `library:` setting of a beets config is not used.

Soulbeet keeps these databases with its own data: in a `beets-libraries` folder next to the database file `DATABASE_URL` names, at each music folder's path. With `DATABASE_URL=sqlite:/data/soulbeet.db`, the library of `/music/Person1` is `/data/beets-libraries/music/Person1/.beets_library.db`.

### Keep the library databases on a local disk

beets stores its library in SQLite, and SQLite cannot lock a file on most network shares (NFS, SMB/CIFS). A library database on a share makes imports fail with "The beets library database is locked". Your music can stay on the share; only the folder mounted at `/data` needs to be on a local disk:

```yaml
volumes:
  - ./data:/data              # local disk: soulbeet.db, secret.key, beets-libraries
  - /mnt/nas/music:/music     # the share
```

Earlier versions kept the database at the root of the music folder (`/music/Person1/.beets_library.db`), so on the share. The first start after the upgrade moves it to `beets-libraries`, with its journal file if it has one; for a folder that is not there at that start, its first import, delete or replace does.

If you mount a music folder at another path later, move its folder under `beets-libraries` to the new path as well. Otherwise Soulbeet starts an empty library for it, and beets no longer sees the tracks already there as duplicates.

If you delete a music folder to start it over, delete its folder under `beets-libraries` too (for `/music/Person1`, `/data/beets-libraries/music/Person1`). Otherwise beets still lists the deleted tracks and skips them as duplicates.

### Tracks whose file is gone

Each Discovery profile folder (`Discovery/Balanced` and the others) has a library of its own too. Soulbeet drops a track from its folder's library when it deletes the file, and from its profile folder's library when a discovery track is removed, expires or is kept. On each start it also drops, from the library of each music folder and of each of its profile folders, the tracks whose file is gone. Only the library entries go: no file is touched. A library some of whose tracks are gone and none of whose tracks has its file there is left as it is, since a share that is not mounted looks the same, and so is a track in a folder Soulbeet cannot read.

### When the library database is locked

A lock can also come from another program that has the database open, such as a `beet` command you started or a backup tool. When an import finds the database locked, its row says "The beets library database is locked" and when Soulbeet tries again: it imports the files again by itself after 1, 5 and 15 minutes. **Retry import** works during that time too. When the last try still finds it locked, close the other program or move the data folder to a local disk, then click **Retry import**. A restart starts the tries over, from the 1 minute wait.

### Interacting with your library

Since each folder has its own database, tracks cannot be compared across libraries. You can still use the `beets` CLI on each library, for example to add tracks outside of Soulbeet and keep them in sync with your library.

To manually interact with a library (list tracks, modify tags, remove items, etc.), point `beet` at the folder's database with `-l` and at the folder itself with `-d`. beets stores each track's path relative to the folder, so without `-d` it looks for the files in the wrong place:

```bash
# List all tracks in a library
beet -l /data/beets-libraries/music/Person1/.beets_library.db -d /music/Person1 ls
```

If using Docker, run these commands inside the container:
```bash
docker exec -it <container_name> beet -l /data/beets-libraries/music/Person1/.beets_library.db -d /music/Person1 ls
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
