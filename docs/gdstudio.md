# NetEase (GD Studio)

This download source finds tracks in NetEase Cloud Music's catalogue through GD Studio's public API (`music-api.gdstudio.xyz`) and downloads them from NetEase's file servers inside Soulbeet. It needs no account, no key and nothing to install. It is off until an admin turns it on in Settings > Config > Connections.

## The operator's terms

GD Studio's API page says, in Chinese: "本站资源来自网络，仅限本人学习参考，严禁下载、传播或商用，如侵权请与我联系删除。继续使用将视为同意本声明". In English: the site's resources come from the internet and are for personal study only; downloading, spreading and commercial use are strictly forbidden; continued use counts as agreeing to this statement. It also says, in English: "This platform is for study purposes only. Do NOT use it commercially!", and it asks anyone using the API to credit "GD音乐台(music.gdstudio.xyz)".

So the page forbids exactly what this source does. Soulbeet builds it as an opt-in source and leaves the decision to the admin of each install. The page is licensed CC BY-NC 4.0, which covers GD Studio's own page and code, not the music. The files are NetEase's licensed copies, served outside NetEase's apps by a third party.

Soulbeet credits the operator as asked: every NetEase group in the source list reads "NetEase via GD音乐台 (music.gdstudio.xyz)".

## What it offers

NetEase holds lossless copies of most mainstream and recent releases, lossy ones of some older or smaller albums, and nothing of catalogue it does not license. For an artist it does not license, a search finds covers and karaoke versions; Soulbeet drops a row whose title names another version (cover, karaoke, live, remix, instrumental, 伴奏, 原唱, 翻唱, 钢琴版, 女声版, 纯音乐, 现场 and others the wanted title does not name), a row by another artist, and a file 30 seconds or more off the wanted length.

A file is offered only when:

- GD Studio names it on one of NetEase's file servers (`*.music.126.net`, over https), with its MD5 as its file name;
- the server confirms the size GD Studio gave, and the MD5 when it sends one;
- its first bytes are the FLAC or MP3 its name says.

Bit depth and sample rate come from the FLAC header. A 24-bit header earns no more than a 16-bit one: some 24-bit masters are upsampled, and one album can mix both. Files are scored on the same scale as Soulseek's, so auto-download picks between them by the same rule.

Artist names can come back in another script (高田みどり for Midori Takada); a row whose title and album match well is kept whatever script its artist is in.

## The call budget

GD Studio publishes a limit of 50 calls in 5 minutes. Soulbeet sends at most 45 in any 5 minutes, one at a time and one second apart at least, for every user and every search, download and status check of the install together, and a call waits for its turn. A call counts when it is sent, and the calls of the last 5 minutes are kept in `<DOWNLOAD_PATH>/.soulful-partial/<install id>/.gdstudio-calls`, so a restart does not start the count again. When GD Studio answers 429 or 503, every call waits a minute. After 5 failed calls in a row, Soulbeet leaves GD Studio alone for a minute. Soulbeet follows no redirect from GD Studio's API.

What a search costs:

| Work | Calls |
|---|---|
| An album of N tracks | 1 + N, up to 3 + N when NetEase names the album differently |
| One track | up to 4 |
| A download started within 20 minutes of its search | 0 |
| A download started later | 1 |
| The status line, when nothing else called for 10 minutes, or a minute after a failed call | 1 |

So about three twelve-track albums fit in 5 minutes. Once it is on, auto-download searches NetEase for every album it looks for, whether NetEase or Soulseek then wins. A search runs at most 150 seconds; one cut short by the budget offers what it checked and says how many tracks it did not check.

## How a search runs

Auto-download searches NetEase next to Soulseek, and Acquire & Play (`POST /api/acquire-and-play`) searches it in the same tier as Soulseek. The manual "Search sources" view, the MCP tools and the archival FLAC upgrade worker search Soulseek only.

1. For an album, the tracks of the album NetEase finds for the artist and the album title. When less than half of the wanted tracks are there, the tracks NetEase finds across albums for the same words, and then the tracks of the album whose name is closest.
2. For one track, the rows NetEase finds for the artist and the title; the best three are checked.
3. For each row that is a wanted track, where NetEase serves it now (one call), and the first 4 KB of its file.

## How a download runs

Downloads run inside Soulbeet, two at a time for the install; a free slot goes to the user running the fewest.

The file URL a search was given is used for 20 minutes; NetEase's servers refuse it 25 minutes after it was named. A later download asks GD Studio again, and the answer must name the same file (same format, size and MD5) as the search found; when it names another one, the download fails with `source_file_changed` and nothing is fetched. A URL refused at the start of a request is asked for again once.

While a file downloads, its bytes are in `<DOWNLOAD_PATH>/.soulful-partial/<install id>/gdstudio/`, so `DOWNLOAD_PATH` must be writable by Soulbeet. A cut answer resumes from where its bytes end, twice at most, so a file gets three requests. The finished file must have the size GD Studio gave and the MD5 it is named by, and be the FLAC or MP3 its name says; otherwise it is deleted and the download fails. NetEase's files carry no tags, so Soulbeet writes the wanted track's title, artist, album, album artist, track number, track total and date into it. The MusicBrainz ids go in only when the file matched closely (title and album 0.8 or more, length within 3 seconds), so another master is not tagged as the release you asked for.

A file lands in its batch folder as `<NN> - <Title> [<NetEase track id>].<ext>`, the title cut so the name fits in 236 bytes, and never replaces a file: when the name is taken, it lands as a copy with a time stamp in its name. beets then renames it.

Downloads are kept in memory. After a restart Soulbeet no longer has the unfinished ones, deletes their partial files, and treats them as lost transfers; a file that had already landed is found and imported.

## Turning it off

Turning it off stops its searches and its unfinished downloads and deletes their partial files. Downloads still waiting on it fail with "NetEase (GD Studio) is turned off in Settings > Config > Connections". Files it finished stay in their batch folders.

## What GD Studio and NetEase see

GD Studio (behind Cloudflare) and NetEase's file servers see your server's IP address; the file servers send it back in a header. Every request carries Soulbeet's User-Agent, which names the project but not you or your server. GD Studio can start requiring signed requests at any time, as its own site already does; Soulbeet then shows the source as unreadable, and it stays so until Soulbeet is updated.
