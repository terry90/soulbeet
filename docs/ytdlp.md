# YouTube Music

The YouTube Music download source finds albums and tracks on YouTube Music when no other source has them, and downloads their audio as Opus at about 130 kbps through [yt-dlp](https://github.com/yt-dlp/yt-dlp). Its files are lossy, always: worse than any lossless file and than a good MP3. It is off until an admin turns it on in Settings > Config > Connections, and it needs the `:full` image.

## YouTube's terms

YouTube's [Terms of Service](https://www.youtube.com/t/terms) do not allow downloading content from YouTube, except where YouTube offers it or gives written permission, nor reaching YouTube by automated means. yt-dlp and ytmusicapi do both. Soulbeet does not ship this source turned on: whether to turn it on on your server is your decision, and so is what you download with it.

## What it offers

Soulbeet asks YouTube Music for the album, then reads the tracklists of the best three albums that name the artist and the album. For a single track it asks for songs. A track is offered when all of these hold:

- it is the release's own audio track (YouTube Music's "Art Track"), never a music video or an upload;
- YouTube Music plays it in your server's country;
- its title shares words with the wanted title, and names no version the wanted title does not name, such as live, remix, sped up, slowed, instrumental or cover;
- its length is within 30 seconds of the MusicBrainz one;
- it scores 0.6 or more on title, artist, length and album, as Soulseek files do.

Between a clean and an explicit version that match as well, the explicit one is taken.

A YouTube file always scores 0.5 for its quality, just under an MP3 of 128 kbps. A complete, exact album then scores about 0.8, and one that lacks a track of twelve, or whose titles match a little less well, still reaches the 0.7 auto-download picks at.

## When it is searched and picked

- Auto-download searches YouTube Music last: only when no other source found something it would pick on its own. Its results come after every other source's, under "Lossy, from YouTube Music", each with a "LOSSY · YOUTUBE MUSIC" label. The label stays on the download row through the import.
- Each user chooses, in Settings > Library, "When only YouTube Music has it": **Never use it** (it is not searched for them at all), **Ask me** (the default: its results are offered to pick from, never picked on their own) or **Download automatically (lossy)**.
- A file from YouTube Music never replaces a copy already in the library: its imports skip what the library has, whatever started them.
- The manual "Search sources" view and the MCP tools search Soulseek only.

## How a download runs

- One video at a time for the whole server, 10 to 20 seconds apart and at most 120 in any hour, which leaves YouTube's guest limit room for the rest of your server's use of YouTube. Users take turns, and one user may have at most 200 downloads waiting.
- yt-dlp takes YouTube's Opus format of 96 kbps or more, or nothing, and ffmpeg copies the stream out of its WebM, without encoding it again.
- The file's length must be within 2 seconds of the one YouTube lists. It is then tagged as the MusicBrainz track it was found for: title, artists, album, track number and date. The MusicBrainz ids go in only when the match is close (title and album scores of 0.8 or more, a length within 3 seconds), since beets trusts them. Its comment reads "Lossy: YouTube Music, Opus 136 kbps, video …".
- The file is fetched and tagged in `DOWNLOAD_PATH/.soulful-partial/<install id>/ytdlp/`, then moved into its batch folder. A finished file never replaces another: when its name is taken it lands as a copy with a time stamp in its name, as slskd does.
- A dropped connection is tried twice more, after 30 seconds and 2 minutes.
- Downloads waiting or running are kept in memory only: after a restart they fail as lost, and you start them again. A file that had landed stays in its folder, and the import finds it.

## Rate limits and sign-in checks

YouTube limits what one address may fetch without an account, and it asks some servers to sign in. Neither can be fixed from Soulbeet.

- When YouTube answers "try again later" or 429, every YouTube Music search and download waits an hour. The downloads waiting fail at once, with the time to wait, and the status line reads "YOUTUBE MUSIC RATE-LIMITED · LOSSY" with "YouTube rate-limited this server until 14:32 UTC".
- When YouTube asks the server to sign in to confirm it is not a bot, every search and download waits 6 hours; a check that passes before then ends the wait. Three refused downloads (HTTP 403) within an hour count as such a request. yt-dlp's users report that servers in data centres are asked far more often than home connections.

## Status line

The status line reads what YouTube answered Soulbeet's last search or download in the last 30 minutes. Only when there was none does Soulbeet check on its own: it fetches the first 10 KB of a Creative Commons track (Kevin MacLeod, "Monkeys Spinning Monkeys", CC BY 4.0), about 6 requests. It can say:

- "Not in this image: use the :full image (… missing)": yt-dlp, yt-dlp-ejs, ytmusicapi, deno or ffmpeg is not there or does not run. Nothing is asked of YouTube.
- "YouTube rate-limited this server until …", shown as RATE-LIMITED.
- "YouTube asks this server to sign in …".
- "YouTube did not answer".

yt-dlp breaks when YouTube changes, every few weeks, until a newer yt-dlp is released. Each image pins one yt-dlp release, so keep the image updated; Soulbeet logs a warning when the yt-dlp it has is more than 60 days old.

## What YouTube sees, and what runs on your server

- YouTube sees your server's address. Requests carry yt-dlp's and ytmusicapi's own client headers, and no account, cookie or anything about your users.
- yt-dlp and ytmusicapi run in a Python helper Soulbeet starts for each search or download, with a fixed command line, no shell, and an environment holding only its own folders: it sees no key and no password. Each run is in a process group of its own, killed as a whole when it ends, is cancelled, times out (2 minutes without output, 10 minutes in all for a download), or the source is turned off.
- YouTube's player code, which yt-dlp must run, runs in deno with no permissions: no files, no network, no programs.
- What the helper prints is never logged whole. Messages keep no URL, since YouTube's media URLs carry your server's address.

## Turning it off

Turning it off stops its searches and downloads, kills their helpers and deletes their staging files. Downloads still waiting on it fail with "YouTube Music is turned off in Settings > Config > Connections". Files it finished stay in their batch folders.

## Image

Only the `:full` image has the tools: yt-dlp, yt-dlp-ejs, ytmusicapi and deno in `/opt/ytdlp`, apart from beets' packages, pinned by hash in `build/requirements-ytdlp.txt`, and ffmpeg. They add about 120 MB, 47 MB compressed. The `light` and `medium` images do not have them, and grow by nothing. See [configuration.md](configuration.md#youtube-music) for the paths.
