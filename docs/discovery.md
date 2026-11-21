# Discovery

This page sets up discovery, which generates personalized playlists from what you play and star in Navidrome and pushes them to Navidrome, and explains how it runs.

## Navidrome configuration

Your Soulbeet folder paths must point to the same physical directories that Navidrome's music library uses. The mount paths inside each container can differ (e.g. Soulbeet at `/music`, Navidrome at `/media/music`), as long as they map to the same files on the host. Soulbeet learns where each Navidrome library is from the files it imports; set `NAVIDROME_MUSIC_PATH` to where Soulbeet sees Navidrome's default library to give it that root from the start (see [configuration.md](configuration.md)).

## Soulbeet configuration

1. Go to **Settings > Library** in the Soulbeet web UI
2. Optionally add your **ListenBrainz username**. Soulbeet then reads your ListenBrainz history too. The token is optional: with one, ListenBrainz allows more requests and ranks an artist's top tracks itself
3. Optionally turn on **Also use Deezer for recommendations**. This sends the names of artists you play to Deezer, which has not approved this use, so it is off until you turn it on
4. **Enable Discovery** on a folder and pick your profiles (Conservative, Balanced, Adventurous)
5. Optionally customize playlist names for each profile
6. Hit **Generate** to run the first batch

Discovery starts from the songs you played, starred or rated 4 or 5 stars in Navidrome. Songs you rated 1 or 2 stars never count. With nothing played or starred yet, a run stops and the Discovery page says "No listening data yet. Play or star music in Navidrome." The page also lists the sources that fed the last run and any that failed.

## How it works

- Discovery creates a `Discovery/` directory inside your chosen folder, with subdirectories per profile (e.g., `Discovery/Balanced/`, `Discovery/Adventurous/`).
- Tracks are downloaded from Soulseek, imported via beets into the profile subdirectory, and tagged properly.
- A smart playlist is created in Navidrome (via the native API) for each profile, filtered by the folder path. The playlist auto-updates as Navidrome scans new files. Later runs change that same playlist in place: they set its name from Settings and its path filter when either differs, and leave its comment and public flag as you set them in Navidrome. Its id never changes, so players keep it. A new one is made only when Navidrome no longer has it.
- Every 6 hours, an automation task syncs ratings, creates playlists if missing, and regenerates expired discovery batches. Recommendations for the profiles you selected are refreshed on their own 6-hour cycle, only for users with discovery on and a discovery folder chosen. The cycle starts at a random point in the first 6 hours after Soulbeet starts.

## Auto-delete

When enabled (Settings > Library > Auto-delete), 1-star tracks are deleted from disk during rating sync. Soulbeet finds each track's file from Navidrome's library path, so it needs the library root (learned from imports, or `NAVIDROME_MUSIC_PATH`). A track another listener rated above 1 star, starred or keeps in a playlist is kept, and so is any track Soulbeet cannot check; Settings > Library > Deletion Checks lists what was kept and why. [mcp.md, "What every deletion checks"](mcp.md#what-every-deletion-checks) lists every check.
