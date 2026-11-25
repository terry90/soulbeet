# Download backends

A download backend finds files and fetches them. slskd (Soulseek) is the default one and is always on. Every other backend is off until an admin turns it on. This page is for whoever adds a backend: what the app expects of it, where it plugs in, and what turning it off does.

## The contract

A backend implements `DownloadBackend` (`lib/soulbeet/src/traits.rs`). The app relies on this:

- `id()` is the registry id, and the backend is looked up only by it. `id()` and `name()` equal the backend's `BackendMeta` in `lib/shared/src/backends.rs`; a unit test of each backend checks it.
- Every `DownloadableItem` that `poll_search` returns names `id()` in `backend`, and its `title` is the wanted MusicBrainz title, so failover can pair files of the same track.
- `quality_score` is `(shared::download::format_score(extension, bitrate) + the backend's own terms).min(1.0)`, so every backend's files sit on one scale. A group's score is `soulbeet::matching::score::group_score`.
- Files of a `download` call land at `<DOWNLOAD_PATH>/<destination>/<name>`, `name` being one of `shared::download::landing_names(last segment of DownloadableItem::id)` or a `ticks_copy` of it. That is what `resolve_in_destination` reads.
- `download` gets `owner`, the batch owner's user id. It is opaque and only for sharing the backend fairly between users: never log it, never send it anywhere.
- `QueuedDownload::id` is stable and is the id `get_downloads_for` and `get_download` list the transfer under. Each listed row names the item's `source` and `DownloadableItem::id`, and echoes the `download` call's `request`.
- A succeeded transfer stays listed until `cancel_download` with `remove`. `remove` forgets an ended record and never moves or deletes a file in the destination; the backend's own partial files are its own.
- A backend that keeps transfers in memory loses them on restart; `get_download` then answers None, which reconcile treats as a lost transfer. An ended record may be forgotten 7 days after it ended.
- `queue_position` is the transfer's place in the line it waits in before it starts: the source's queue for a peer backend, the backend's own queue otherwise; `Some(0)` once it is not waiting; None when the backend no longer has the transfer.
- `source_status` says whether this backend can fetch from the source now. `Blocked` means the backend will not give this source's files.
- `health_check` is called every 10 s per open tab. It must cost the remote service at most one request per minute, so cache inside the backend.
- An item `download` cannot send gets no answer, and the caller fails it.

Failure texts the app writes for a backend name its `BackendMeta::service` ("Download never appeared in slskd"). A backend's own errors implement `BackendFault`. A file whose bytes do not match the checksum its source gave fails with `transfer_checksum_mismatch`; a full disk with `download_disk_full` and the user action `free_disk_space`.

## Shared pieces

- `soulbeet::partial`: `PartialDir::open(download_path, instance, backend)` is `<DOWNLOAD_PATH>/.soulful-partial/<instance>/<backend>/`, the instance being `services::instance_id()`. Two installs sharing one downloads folder have different instance ids. The first open takes a lock that another server on the same database cannot get, which the backend reports as its health failure. `partial::land` moves a finished part to its landing name and never replaces a file.
- `soulbeet::tags`: `write_tags` replaces a file's tags (FLAC, MP3, Ogg Opus). Set the MusicBrainz ids only when `close_match` says so.
- `soulbeet::matching`: path, title, length and version matching, and `assign_tracks`, which gives each wanted track the best file no other track took.
- `soulbeet::TransferBook`: records a backend keeps across restarts, in the `backend_transfers` table (`services::transfer_book()`). Each backend prunes its own entries when it is built.

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
2. calls `stop_backend(id)`: every search and unfinished transfer it runs stops, and their partial files are deleted;
3. drops the built backend (`reload_download_backend(id)`), so the next lookup reads the switch again and answers "{name} is turned off in Settings > Config > Connections" (`backend_not_configured`, user action `configure_backend`).

Batches still on it then fail through the monitor's lookup. Finished files and their records stay. Turning a backend on writes the key and drops the built backend, so the next lookup builds it.
