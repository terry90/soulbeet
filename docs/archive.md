# Internet Archive

The Internet Archive download source finds albums and tracks on archive.org and downloads them inside Soulbeet. It needs no account, no key and nothing to install. It is off until an admin turns it on in Settings > Config > Connections.

## What it offers

archive.org holds Creative Commons netlabel releases, 78 rpm transfers, albums artists put there themselves, and many albums users uploaded. Coverage of new and mainstream releases is thin.

A file is offered only when all of these hold:

- it is one of the files its uploader gave (`source: original`), never one of archive.org's own conversions (its "VBR MP3", "Ogg Vorbis" or "64Kbps MP3" copies);
- its item is not stream-only and not restricted, and the file is not private;
- it is an audio file Soulbeet imports (FLAC, MP3, M4A, Ogg, AAC, WAV, WMA, Opus), with a size and an md5 listed;
- its title, track number and length match a track of the MusicBrainz release you searched for, within 30 seconds of its length.

The Live Music Archive (the `etree` collection) is never searched: live tapes are not the studio releases MusicBrainz lists, and many are stream-only or Shorten files.

For an album, an item that has less than half of its tracks is left out. Items are scored like Soulseek folders, on one scale, so auto-download picks between the two by the same rule. archive.org labels every uploaded MP3 "VBR MP3" with no bitrate, so Soulbeet estimates it from the file's size and length and shows it as `~128k`.

## How a search runs

Auto-download searches the archive next to Soulseek. The manual "Search sources" view and the MCP tools search Soulseek only.

One search sends archive.org at most 11 requests, one per second, shared by every user's searches:

1. When the album's MusicBrainz release is known, the items that name it (`urn:mb_release_id`).
2. The items whose creator and title name the artist and the album (or the track), as phrases. When those find nothing, the same words unquoted.
3. The file lists of the 8 best items.

## How a download runs

Downloads run inside Soulbeet, at most 4 at once and 2 from one item. Users share the slots fairly: a free slot goes to the user running the fewest.

Each file is checked against the md5 archive.org lists before it is put in its batch folder. A file whose bytes do not match is downloaded once more from the start; a second mismatch fails it with `transfer_checksum_mismatch` and nothing is kept. A finished file never replaces another: when its name is taken it lands as a copy with a time stamp in its name, as slskd does.

While a file downloads, its bytes are in `<DOWNLOAD_PATH>/.soulful-partial/<install id>/archive/`, so `DOWNLOAD_PATH` must be writable by Soulbeet, as it already is for imports. A cut download resumes from where its bytes end, and asks archive.org for the same version of the file (`If-Range`); if the file changed, it starts again. Each download is recorded in Soulbeet's database before it starts, so after a restart it resumes. A download's new state (landing, finished, failed) goes in the database before Soulbeet shows it; while the database cannot be written, the download waits and tries again. A file that landed before a restart is never downloaded again: when it is no longer in its batch folder, the download fails with `file_not_found_after_download`. Once its file is imported, its record goes; a record left over is dropped when Soulbeet starts 7 days after the download ended.

When archive.org answers 429 or 503, every download and search waits: a download as long as archive.org asks (`Retry-After`, else 30 seconds, doubled each time), at most 5 times; a search as long as its own retry waits. A wait of more than 10 minutes fails the download with `transfer_rejected_peer_busy` instead of holding a slot. Download requests to archive.org are spaced one second apart with the searches' requests.

The status line reads what archive.org answered Soulbeet's last request, when that was less than a minute ago; only when nothing was asked for a minute does Soulbeet send a check of its own. Whether an item is still there is asked at most once a minute per item.

## Turning it off

Turning it off stops its searches and its unfinished downloads, deletes their partial files and forgets their records. Downloads still waiting on it fail with "Internet Archive is turned off in Settings > Config > Connections". Files it finished stay in their batch folders.

## What archive.org sees

archive.org sees your server's IP address, and every request carries Soulbeet's User-Agent, which names the project but not you or your server. It publishes no rate limit; it blocks heavy automated traffic, and a block would hit every user of your server.

## Rights

Each item's rights are set by its uploader, and archive.org does not guarantee them: its terms put using the collections lawfully on you ([archive.org's rights page](https://help.archive.org/help/rights/)). Some uploads are copies of commercial albums, and archive.org removes items under its takedown process; a download of a removed file fails with "archive.org no longer has this file".

A FLAC upload can be a lossy file converted to FLAC. Soulbeet does not check for that.
