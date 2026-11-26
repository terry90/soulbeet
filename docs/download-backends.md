# Download backends

A download backend finds files and fetches them. slskd (Soulseek) is the default one and is always on. Every other backend is off until an admin turns it on. This page is for whoever adds a backend: what the app expects of it, where it plugs in, and what turning it off does.

The backends in this build:

- `slskd`, Soulseek through the user's slskd server ([configuration.md](configuration.md#settings-in-the-web-ui)).
- `archive`, the Internet Archive ([archive.md](archive.md)): searches archive.org and downloads in Soulbeet's own engine, with partial files in `PartialDir` and its transfers in the transfer book, so a restart resumes them.
- `ytdlp`, YouTube Music ([ytdlp.md](ytdlp.md)): lossy only. A Python helper runs ytmusicapi and yt-dlp, one child process per call; its queue is in memory, so a restart loses what was not finished, and its staging folders are in `PartialDir`.
- `gdstudio`, NetEase through GD Studio's API ([gdstudio.md](gdstudio.md)): searches and downloads in Soulbeet's own engine under a call budget for the whole install, with partial files in `PartialDir`. Its transfers are kept in memory only: a restart loses the unfinished ones, which reconcile then treats as lost, and the engine clears its partial folder when it starts.

## The contract

A backend implements `DownloadBackend` (`lib/soulbeet/src/traits.rs`). The app relies on this:

- `id()` is the registry id, and the backend is looked up only by it. `id()` and `name()` equal the backend's `BackendMeta` in `lib/shared/src/backends.rs`; a unit test of each backend checks it.
- A search still running after 180 s is given up on: the app calls `stop_search` on it, which ends whatever it still asks of its service.
- Every `DownloadableItem` that `poll_search` returns names `id()` in `backend`, and its `title` is the wanted MusicBrainz title, so failover can pair files of the same track.
- `quality_score` is `(shared::download::format_score(extension, bitrate) + the backend's own terms).min(1.0)`, so every backend's files sit on one scale. A group's score is `soulbeet::matching::score::group_score`.
- Files of a `download` call land at `<DOWNLOAD_PATH>/<destination>/<name>`, `name` being one of `shared::download::landing_names(last segment of DownloadableItem::id)` or a `ticks_copy` of it. That is what `resolve_in_destination` reads.
- `download` gets `owner`, the batch owner's user id. It is opaque and only for sharing the backend fairly between users: never log it, never send it anywhere.
- `QueuedDownload::id` is stable and is the id `get_downloads_for` and `get_download` list the transfer under. Each listed row names the item's `source` and `DownloadableItem::id`, and echoes the `download` call's `request`.
- A succeeded transfer stays listed until `cancel_download` with `remove`. `remove` forgets an ended record and never moves or deletes a file in the destination; the backend's own partial files are its own.
- A backend that keeps transfers in memory loses them on restart; `get_download` then answers None, which reconcile treats as a lost transfer. An ended record may be forgotten 7 days after it ended.
- `queue_position` is the transfer's place in the line it waits in before it starts: the source's queue for a peer backend, the backend's own queue otherwise; `Some(0)` once it is not waiting; None when the backend no longer has the transfer.
- `source_status` says whether this backend can fetch from the source now. `Blocked` means the backend will not give this source's files.
- `health_check` is called every 10 s per open tab. A backend that calls a third-party service costs it at most one request per minute, so it caches inside the backend, or answers from its own last request to the service when that is recent (the Internet Archive does). slskd is the user's own server and is checked live.
- Failures, their causes and health answers are shown to every user and written to the log, so they never hold a key, a password or a URL with credentials in it.
- An item `download` cannot send gets no answer, and the caller fails it.

A backend whose meta says `Delivery::LossyOnly` is a last resort, by its meta and never by its id:

- auto-download searches it only when no group of the other backends would be picked (a score of `AUTO_SELECT_SCORE_THRESHOLD` or more), and lists its groups after every other group;
- one of its groups is picked without the user only when the user's `lossy_fallback` setting is `auto`; with `never` it is not searched at all;
- each of its batches gets `DuplicateRule::Skip` (`DuplicateRule::for_delivery`), whatever started it, so beets skips what the library has and no lossy file replaces a copy.

Failure texts the app writes for a backend name its `BackendMeta::service` ("Download never appeared in slskd"). A backend's own errors implement `BackendFault`. A file whose bytes do not match the checksum its source gave fails with `transfer_checksum_mismatch`; a full disk with `download_disk_full` and the user action `free_disk_space`.

## Shared pieces

- `soulbeet::partial`: `PartialDir::open(download_path, instance, backend)` is `<DOWNLOAD_PATH>/.soulful-partial/<instance>/<backend>/`, the instance being `services::instance_id()`. Two installs sharing one downloads folder have different instance ids. The first open takes a lock that another server on the same database and downloads folder cannot get. The lock covers one (database, downloads folder) pair: two servers on one database are not supported, whatever their downloads folders.
- A backend writes a partial file only through `PartialDir::open_part(transfer_id, resume)`. `part(transfer_id)` is its path, for reading its length or checksum. `open_part` refuses a part that already landed (a landing cut between its link and its removal) with `AlreadyExists`; the backend then calls `land` again, which only removes the part. Close the file before `land`, and never write through a handle opened before it.
- `partial::land` moves a finished part to its landing name and never replaces a file. It returns once the link and the part's removal are on disk, so the backend may then record the file landed.
- `soulbeet::polite::HostGate::of(base_url)`: the pace and the hold every client of one host shares. A backend that sends its own requests to a host its searches also use waits on it (`wait_for_turn`) and holds it on a 429 or 503 (`hold`), so a rate limit seen by one request holds them all.
- `soulbeet::tags`: `write_tags` replaces a file's tags (FLAC, MP3, Ogg Opus). It runs on a part before `land`, and refuses a file with more than one link. The MusicBrainz ids go in only when the match facts given with them make a close match (title and album scores of 0.8 or more, a known length within 3 s).
- `soulbeet::matching`: path, title, length and version matching, and `assign_tracks`, which gives each wanted track the best file no other track took.
- `soulbeet::TransferBook`: records a backend keeps across restarts, in the `backend_transfers` table (`services::transfer_book()`).
- `soulbeet::process::ProcessGroup`: a child process in a process group of its own, killed as a whole when the guard drops. beets and the YouTube Music helper run in one, so a cancel, a timeout or a turn-off ends every process they started.

## Building a backend

`init_download_backend` builds a backend's handle for the registry. A handle is built again whenever the registry drops it: when an admin turns the backend on, when its own settings are saved, and after any lookup error. The backend's process-wide engine (its workers, its queue, its running transfers) outlives those rebuilds. So:

- Resume, prune and `clear_except` run once, when the engine is first created, never on a rebuild of the handle. A rebuild while transfers run must not touch their parts or their book entries.
- An engine that keeps partial files opens its `PartialDir` before anything else. On any error (another server holds the lock, the downloads folder cannot be written) the backend reports it as its health failure and touches neither the book nor any file.
- A build holds up the lookups of its own id only, so keep it short and never look the same backend up from inside it.
- The Internet Archive engine (`soulbeet::archive::ENGINE`) is the model, and the YouTube Music (`soulbeet::ytdlp::ENGINE`) and GD Studio (`soulbeet::gdstudio::ENGINE`) ones follow it: the build starts it once and wraps each handle around it; the handle keeps the generation it was built in, and the engine's stop bumps the generation, so an older handle's `download` and `start_search` fail with `backend_not_configured`. When the backend is turned off before anything built it since a restart, its stop arm still forgets the unfinished transfers the previous run left in the book, and deletes their partial files. GD Studio keeps no book, so its arm deletes every partial file the previous run left.

## Where a backend plugs in

Each of these takes one block per backend:

- `lib/shared/src/backends.rs`: the backend's `BackendMeta` const and its entry in `ALL`.
- `lib/shared/src/lib.rs` and `lib/soulbeet/src/lib.rs`: `pub mod <id>;`.
- `lib/shared/src/failure.rs`: its `Service` variant and the wire test entry. `api/src/core/mod.rs::logged` names each `Service` too.
- `api/src/services.rs`: its id in `downloaders`, its arm in `init_download_backend`, and its arm in `stop_backend`.
- `ui/src/components/settings/backends.rs`: an `extra_settings` arm, only for a backend with a settings form of its own.
- `e2e/stubs/src/main.ts` (import and listen), `e2e/compose.e2e.yml` (env in both app services, port).
- `docs/configuration.md` (a subsection under "Download sources") and `docs/mcp.md` (its `Service` line).

## Turning a backend off

The switch is the app_config key `<id>_enabled`, `"true"` while on; no key is off. Only an admin changes it, through `set_backend_enabled`. Turning a backend off:

1. deletes the key;
2. drops the built backend (`reload_download_backend(id)`). This waits for a build in flight, so from then on every lookup reads the switch and answers "{name} is turned off in Settings > Config > Connections" (`backend_not_configured`, user action `configure_backend`);
3. calls `stop_backend(id)`: every search and unfinished transfer it runs stops, their partial files are deleted and their transfer book entries forgotten. Entries of finished transfers stay. A handle taken before the drop is refused `download` and `start_search` from then on, until the backend is built again.

Batches still on it then fail through the monitor's lookup with that text. Finished files and their records stay. Turning a backend on writes the key and drops the built backend, so the next lookup builds it. Saving the slskd settings drops only slskd.
