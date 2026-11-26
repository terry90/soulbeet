# Discovery

This page sets up Discovery 2.0, which generates personalized playlists, Seasonal Packs, Moment Mixes, and Seed Radio from what you play and star in Navidrome, and explains how curation, reconciliation, and auto-delete work.

## Navidrome configuration

Your Soulbeet folder paths must point to the same physical directories that Navidrome's music library uses. The mount paths inside each container can differ (e.g. Soulbeet at `/music`, Navidrome at `/media/music`), as long as they map to the same files on the host. Soulbeet learns where each Navidrome library is from the files it imports; set `NAVIDROME_MUSIC_PATH` to where Soulbeet sees Navidrome's default library to give it that root from the start (see [configuration.md](configuration.md)).

## Soulbeet configuration

1. Go to **Settings > Library** in the Soulbeet web UI.
2. Optionally add your **ListenBrainz username**. Soulbeet then reads your ListenBrainz history too. The token is optional: with one, ListenBrainz allows more requests and ranks an artist's top tracks itself.
3. Optionally turn on **Also use Deezer for recommendations**. This sends the names of artists you play to Deezer, which has not approved this use, so it is off until you turn it on.
4. **Enable Discovery** on a folder and pick your rolling profiles (Conservative, Balanced, Adventurous).
5. Optionally customize playlist names for each profile and set your cultural preferences (home country, preferred languages, and local/international ratio) for Seasonal Packs.
6. Hit **Generate** to run the first batch.

Discovery starts from the songs you played, starred or rated 4 or 5 stars in Navidrome. Songs you rated 1 or 2 stars never count. With nothing played or starred yet, a run stops and the Discovery page says "No listening data yet. Play or star music in Navidrome." The page also lists the sources that fed the last run and any that failed.

## Discovery 2.0 surfaces

### Rolling profiles (Conservative, Balanced, Adventurous)

- Discovery creates a `Discovery/` directory inside your chosen folder, with subdirectories per profile (e.g., `Discovery/Balanced/`, `Discovery/Adventurous/`).
- Tracks are downloaded, imported via beets into the profile subdirectory, and tagged.
- A smart playlist is created in Navidrome (via the native API) for each profile. It holds the songs of your own `Discovery/<profile>/` folder only, by its path in Navidrome's library, so users who share one library each get their own tracks. To know that path Soulbeet needs the library's root: it learns it from an import, from one of your discovery tracks once Navidrome lists it, or from `NAVIDROME_MUSIC_PATH`. Until then no playlist is made. The playlist auto-updates as Navidrome scans new files. Later runs change that same playlist in place: they set its name from Settings and its library and path filter when either differs, and leave its comment and public flag as you set them in Navidrome. Its id never changes, so players keep it. A new one is made only when Navidrome no longer has it.

### Seasonal Packs

Seasonal Packs are calendar-windowed contextual collections (such as `winter_holiday` and `new_years_eve`, plus custom admin or user mixes) reconciled as Navidrome smart playlists under `Discovery/Seasonal/<slug>/`, with automatic post-expiry cleanup once a seasonal window ends.

- **Cultural Taste DNA and steering**: each user has a `UserCultureProfile` (`home_country`, `preferred_languages`, `local_culture_ratio`, plus inferred regional weights and tags from listening history). You can steer any active Seasonal Pack between more local, balanced, or more international mixes.
- **Core taste isolation (`exclude_from_core_taste`)**: tracks staged for Seasonal Packs are marked `exclude_from_core_taste = 1`, keeping holiday or seasonal listening out of your core rolling profiles so December plays do not skew your January recommendations.

### Moment Mixes

Moment Mixes (built-in presets such as Drive, Morning, Focus, and Late Night, plus custom mixes) materialize on demand as a hybrid blend of owned Navidrome tracks and fresh staged discovery tracks, with optional static playlist sync to Navidrome. They can be launched directly from the Discover page, the Command Palette, Desktop, or Android Auto.

### Triage Mode (Keep / Drop with 5-second undo)

Pending discovery tracks can be triaged directly inside the Studio Web Player, Desktop app, and Android app without switching to Navidrome:

- **Keep (Promote)**: moves the staged track out of `Discovery/` into your permanent library folder via beets.
- **Drop**: hides the track from your triage list immediately and opens a 5-second Undo bar. If you do not click Undo before the timer expires (or once another drop displaces it), Soulbeet commits the removal and records the track in `discovery_rejections` so it is not recommended again.

### On-Keep archival FLAC upgrade

Tracks acquired via a fast lossy path are recorded in `fast_acquisitions` without scheduling any automatic replacement. Only when you explicitly **Keep**, **Star**, or **Promote** a track does Soulbeet enqueue a low-priority background archival upgrade job (polled every 120 seconds, up to 3 attempts) to search for a lossless FLAC copy on an archival backend (Soulseek by default) and import it with guarded replacement, preserving Navidrome song identity, ratings, and playlist memberships.

### Seed Radio

Start an on-demand Seed Radio queue from any track or artist (or press `R` in the Studio player). Seed Radio blends owned library tracks and fresh candidates with structured provenance reasons (`CandidateReason`) explaining why each track was chosen.

## How background runs and reconciliation work

- The Soulseek server silently drops searches that contain a phrase on its exclusion list, often a well known artist name or title. A search no peer answers is sent again with wildcards in place of some words ([mcp.md, Known limits](mcp.md#known-limits) says how). A track whose search found nothing in 3 runs in a row is not searched again.
- After a run imports tracks, Soulbeet starts a Navidrome scan and waits up to two minutes for it. Navidrome lets only admins start one, so for a user whose Navidrome account is not an admin, Soulbeet starts it with another linked account that Navidrome lets start one. When no linked account may, the tracks appear at Navidrome's next own scan (file watcher or scan schedule). [mcp.md, "Navidrome scans"](mcp.md#navidrome-scans) says more.
- **`song_id` reconciliation**: if a newly imported pending discovery track is not yet linked to its Navidrome `song_id` (for example while Navidrome finishes scanning), Soulbeet reconciles pending tracks against Navidrome's index right after generation scans and lazily whenever triage queues, discovery tracks, or contextual mixes are loaded.
- Every 6 hours, an automation task syncs ratings, creates playlists if missing, cleans up expired seasonal packs, and regenerates expired discovery batches. A pending track expires only when that cycle's rating sync read it and found it below your promote threshold: when the sync fails, or the promote of a track you rated fails, the track stays until a later cycle. Recommendations for the profiles you selected are refreshed on their own 6-hour cycle, only for users with discovery on and a discovery folder chosen. The cycle starts at a random point in the first 6 hours after Soulbeet starts.

## Auto-delete

When enabled (Settings > Library > Auto-delete), 1-star tracks are deleted from disk during rating sync. Soulbeet finds each track's file from Navidrome's library path, so it needs the library root (learned from imports, or `NAVIDROME_MUSIC_PATH`). A track another listener rated above 1 star, starred or keeps in a playlist is kept, and so is any track Soulbeet cannot check; Settings > Library > Deletion Checks lists what was kept and why. [mcp.md, "What every deletion checks"](mcp.md#what-every-deletion-checks) lists every check.
