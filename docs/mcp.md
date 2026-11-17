# Connecting Claude to Soulbeet (MCP)

Soulbeet can answer Claude, on claude.ai and in the Claude apps, as an MCP server. You add your Soulbeet as a custom connector and ask Claude for music in plain words: "find these eight carols, get the ones I don't have and make a playlist of them". Claude then checks your library, searches Soulseek, downloads, imports with beets and fills the Navidrome playlist through Soulbeet's tools.

This page is for the person who runs Soulbeet. It covers what your server needs, how to put it behind a reverse proxy, how each person connects, what Claude may and may not do, and what each error means.

## What it is

Soulbeet answers Claude at `https://{your address}/mcp`, for whatever address you serve it at. There is no fixed host name to configure.

Each person connects their own Soulbeet account, and Claude then acts as that person only: their folders, downloads, playlists, ratings and settings.

Through Soulbeet, Claude can:

- read Soulbeet's status: whether slskd, beets and Navidrome work, your folders and their free space;
- read your Navidrome library: search it, find tracks by MusicBrainz id or artist and title, list albums, starred items and playlists;
- look up recordings and releases on MusicBrainz or Last.fm;
- search Soulseek for sources and download a chosen one;
- get a list of tracks or albums into your library in one batch, and follow it;
- decide imports that beets skipped;
- create, rename and edit your playlists, rate and star songs and albums;
- run discovery, keep or remove discovery tracks, and run your rating sync;
- change some of your settings;
- find duplicates and delete music, only once you switch deletion on for that app (see [Deleting music](#deleting-music)).

Long work goes on in Soulbeet after the chat ends: a batch of downloads keeps running, imports finish, and a playlist asked for with the batch keeps filling. In a new chat Claude finds earlier work with `list_batches`.

## Requirements

- **A public name.** Claude connects from Anthropic's servers, not from your browser. The name needs an `A` record, and every address it resolves to must be globally routable: not `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `100.64.0.0/10`, loopback or link-local. A name with only an `AAAA` record does not work.
- **https with a certificate Claude trusts.** Soulbeet refuses Claude's endpoints over plain http, except on a loopback address for local tests.
- **Soulbeet at the root of that name.** Soulbeet cannot run under a sub-path: `https://music.example.com` works, `https://example.com/music` does not.
- **The Security Checks in Settings > Config pass.** Until they do, Claude's endpoints answer `mcp_unavailable`. The two checks are:
  - `SECRET_KEY` is unset (Soulbeet then generates a key) or at least 32 bytes. A shorter one is not used at all: Soulbeet signs sessions with the key in `secret.key` instead. Settings shows: "SECRET_KEY is shorter than 32 bytes, so Soulbeet does not use it and signs sessions with the key stored in secret.key next to the database. Remove SECRET_KEY, or set it to a random value of at least 32 bytes, for example the output of `openssl rand -hex 32`. Setting a new SECRET_KEY ends every session and makes the stored Navidrome passwords unreadable: each user is asked to sign in again once."
  - The seeded `admin` account no longer has the password `admin`. Settings shows: "The seeded admin account still has the password 'admin'. Change it in Settings > Users."

Two environment variables concern Claude:

- `MCP`: `on` or `off`, in any case, for the OAuth and `/mcp` endpoints Claude uses; any other value turns them off and logs a warning. Unset is `on`.
- `PUBLIC_URL`: optional. The public address, `https://host` or `https://host:port`, no path. When it is set, Soulbeet uses it as its address and does not read the address from request headers. If it is set and cannot be used, nobody can sign in until it is fixed or removed, and Settings > Config says why.

One more matters for deletion and for knowing whether Navidrome sees your folders:

- `NAVIDROME_MUSIC_PATH`: optional. Where Soulbeet sees the root of Navidrome's default library, when Soulbeet mounts it at another path than Navidrome does (for example Navidrome at `/music` and Soulbeet at `/media/music`). Without it Soulbeet learns where a library is from the files it imports.

Navidrome versions, as Soulbeet checks them:

- **0.60 or newer** for deleting music, for replacing a library copy, and for the deletions of the rating sync. Soulbeet reads the version Navidrome reports. Below 0.60, or when the version cannot be read, every deletion of a library file is refused and the file is kept.
- **0.55 or newer** for the native song reads used by `find_in_library`, the deletion checks and the wait for Navidrome to list a new file. This is not a version check: an older Navidrome answers without fields Soulbeet needs, so the read fails.

slskd must be 0.26 or newer, as for the web app.

## Behind a reverse proxy

Soulbeet works out its own address from each request, in this order:

1. `PUBLIC_URL`, when set. Nothing else is read.
2. The `host=` and `proto=` of the first element of a `Forwarded` header.
3. The first value of `X-Forwarded-Host` and of `X-Forwarded-Proto`.
4. The `Host` header.

Host and scheme are read separately, each from the first of these that has it. A named host with no scheme hint counts as https; `localhost` and IP addresses count as http.

The proxy must:

- **Set the forwarded headers itself, and never pass on the ones a client sent.** Soulbeet reads the first `Forwarded` element and the first `X-Forwarded-*` value, so a client that sends its own would choose the address Soulbeet issues tokens for. Each example below overwrites or removes them. Where a proxy cannot promise that, set `PUBLIC_URL`, and the headers are not read.
- **Send the public host and the https scheme**, through `Host` or `X-Forwarded-Host`, and `X-Forwarded-Proto: https`.
- **Not buffer `text/event-stream` answers.** Soulbeet sends them with `X-Accel-Buffering: no`.
- **Let one answer take 300 seconds.** A tool call can wait up to 180 seconds on the server, and Claude allows a call 240 seconds. Clients on protocol 2025-11-25 or older, which per Claude's documentation includes claude.ai, get the response headers at once and a keep-alive every 15 seconds while a tool waits. Clients on protocol 2026-07-28 get no byte until the tool answers.
- **Pass WebSocket upgrades**, for the web app's live download updates.

Each example serves `music.example.com` and sends requests to Soulbeet at `soulbeet:9765`.

### Caddy

```caddyfile
music.example.com {
	reverse_proxy soulbeet:9765 {
		header_up -Forwarded
	}
}
```

Caddy keeps `Host`, sets `X-Forwarded-Proto` and `X-Forwarded-Host` in place of what an untrusted client sent, flushes event streams at once, has no read timeout by default and proxies WebSockets. `header_up -Forwarded` removes a client's `Forwarded` header. If another proxy sits in front of Caddy, list it in `trusted_proxies` so Caddy keeps that proxy's `X-Forwarded-*` values.

### nginx

```nginx
map $http_upgrade $connection_upgrade {
    default upgrade;
    ''      close;
}

server {
    listen 443 ssl;
    server_name music.example.com;
    ssl_certificate     /etc/letsencrypt/live/music.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/music.example.com/privkey.pem;

    location / {
        proxy_pass http://soulbeet:9765;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Host $host;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header Forwarded "";
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection $connection_upgrade;
        proxy_read_timeout 300s;
    }
}
```

By default nginx sends `Host $proxy_host` (here `soulbeet:9765`), sends `Connection: close`, passes a client's `X-Forwarded-*` and `Forwarded` headers on, and ends an answer that sends nothing for 60 seconds. The lines above fix each of those; `proxy_set_header Forwarded "";` removes the header. nginx honours Soulbeet's `X-Accel-Buffering: no`.

### Traefik

Labels on the Soulbeet container, with a `websecure` entrypoint on port 443 and a certificate resolver named `letsencrypt`:

```yaml
services:
  soulbeet:
    image: docker.io/docccccc/soulbeet:latest
    labels:
      - traefik.enable=true
      - traefik.http.routers.soulbeet.rule=Host(`music.example.com`)
      - traefik.http.routers.soulbeet.entrypoints=websecure
      - traefik.http.routers.soulbeet.tls.certresolver=letsencrypt
      - traefik.http.routers.soulbeet.middlewares=soulbeet-no-forwarded
      - traefik.http.middlewares.soulbeet-no-forwarded.headers.customrequestheaders.Forwarded=
      - traefik.http.services.soulbeet.loadbalancer.server.port=9765
```

Traefik passes `Host` on (`passHostHeader` is true by default), sets `X-Forwarded-Proto` and `X-Forwarded-Host`, and drops the `X-Forwarded-*` headers of a client it does not trust. It does not drop a client's `Forwarded` header, so the middleware above removes it (an empty value removes a header). Traefik flushes streaming answers at once. Its entrypoint `readTimeout` (60 seconds by default) limits reading the request, not the answer.

### Cloudflare tunnel

In `cloudflared`'s configuration:

```yaml
ingress:
  - hostname: music.example.com
    service: http://soulbeet:9765
  - service: http_status:404
```

and in Soulbeet's environment:

```yaml
    environment:
      - PUBLIC_URL=https://music.example.com
```

Leave `httpHostHeader` unset, so the public name reaches Soulbeet as `Host`; the web app's live updates check it. Cloudflare sets `X-Forwarded-Proto`. Set `PUBLIC_URL` as well: Soulbeet then reads no forwarded header, whatever a client sent through the tunnel.

Two Cloudflare limits:

- Cloudflare answers 524 when the origin sends nothing for 125 seconds. Clients on protocol 2026-07-28 get no byte until a tool answers, so for them a wait longer than about 120 seconds can end in a 524 while the work goes on in Soulbeet. Clients on 2025-11-25 or older get a keep-alive every 15 seconds and are not affected.
- WAF and bot rules must let Claude's addresses, `160.79.104.0/21`, through.

## Check the address

Admins find **Remote access** in Settings > Config. It shows:

- the address Soulbeet derived for the page's own request ("Soulful sees this page's request at {address}"); open Settings at your public address, so the request passes through the same proxy as Claude's;
- where the host and the scheme came from: `forwarded`, `x_forwarded_host`, `host`, `authority`, `public_url` or `missing` for the host, and `forwarded`, `x_forwarded_proto`, `local_host_is_http`, `named_host_is_https`, `public_url` or `unread` for the scheme;
- advice lines, which compare that address with the one your browser is on:
  - "Claude (MCP) access is turned off: {why}."
  - "Claude (MCP) access stays off until the security checks in Settings pass."
  - "PUBLIC_URL pins the address to {address}; request headers are not read."
  - "Your browser is on {browser host}, but the request reached Soulful naming {host}. Have the proxy send X-Forwarded-Host (or Forwarded host=) with the public name."
  - "Your browser is on {browser host}, but the proxy names {host} in X-Forwarded-Host. Check the proxy's forwarded host." (or "in Forwarded")
  - "Your browser uses https, but Soulful sees http: have the proxy send X-Forwarded-Proto: https (or Forwarded proto=https)."
  - "Claude cannot use this address: it is plain http."
  - "Soulful cannot tell this request's address: {problem}"
  - "This address works for Claude: add https://music.example.com/mcp as a custom connector."
- **Latest connection requests**: requests to `/.well-known/`, `/oauth/` and `/mcp`, with the time, the address derived, where it came from, the `Host`, `X-Forwarded-Host`, `X-Forwarded-Proto` and `Forwarded` headers as received, and the status with its refusal code. Soulbeet keeps the latest 50 of each of three kinds: refusals that keep Claude out (`origin_not_allowed`, `https_required`, `mcp_unavailable`, `mcp_disabled`, `address_unreadable`), other refused or failed requests, and answered ones. The list is in memory and starts empty when Soulbeet starts.

## Connect from claude.ai and the phone app

1. On claude.ai, open **Customize > Connectors**, then **Add > Add custom connector**.
2. Give it a name, for example "Soulbeet", and the address `https://music.example.com/mcp`.
3. Keep the options marked **Detected**. Either OAuth client choice works: "Use Claude's published identity" and "Register automatically" are both supported.
4. Click **Add**, then **Connect**. Claude opens Soulbeet in your browser.
5. If you are not signed in, Soulbeet shows its sign-in page (`/login/return?return_to=...`). Sign in with your Soulbeet account, or with your Navidrome account when Soulbeet uses Navidrome sign-in.
6. Soulbeet then asks:

   > **Connect claude.ai to Soulful**
   >
   > claude.ai wants to use Soulful as {your user name}.
   >
   > Once allowed, it can see your library, downloads and settings, search for music, download into your folders, and manage your playlists and ratings. It cannot delete music unless you turn that on for it in Settings > Connected apps.
   >
   > When you answer, Soulful sends you back to claude.ai.

   When the app names itself, the page adds "It calls itself "{name}"." For an app whose every return address is on your own computer, it adds "This app runs on a computer (it returns to {host}). Allow it only if you started it yourself." Click **Allow**. **Deny** sends you back with the connection refused.

On Team and Enterprise plans an Owner adds the connector in **Organization settings > Connectors**, and then each member connects with their own Soulbeet account. On the Free plan you can have one custom connector.

In the phone app: connectors connected on the web work in the mobile app. Turn one on for a chat with **+ > Connectors**. Claude asks before it calls a tool (**Allow once** or **Always allow**), and you can set what it may call without asking on the connector's page.

## Accounts

Every person connects their own Soulbeet account, and an app connected by one person acts as that person. Claude sees their folders, downloads, playlists, ratings and settings, and reads Navidrome as their Navidrome account sees it. No tool manages users, folders or the slskd settings, so an admin's Claude cannot change those either.

How accounts appear depends on `REGISTRATION` (see the README): with Navidrome sign-in (`navidrome`, the default when `NAVIDROME_URL` is set), a Navidrome user's Soulbeet account is created the first time they sign in, which can be the sign-in of the connection itself. With `admin`, an admin creates the accounts; with `open`, anyone can register.

A user whose account is not linked to Navidrome can still use the tools that do not read Navidrome; those that do answer `navidrome_not_linked`.

## Connected apps

**Settings > Users > Connected apps** lists the apps you connected. Each row shows the app's host (for example `claude.ai`), "It calls itself "{name}"." when the app gave a name, "Last used {date}" or "Never used", the **Allow deleting music** box (only on your own apps) and **Disconnect**.

- **Allow deleting music** is off for every new connection. Turning it on asks first:

  > Let {app host} delete music from your library? Before deleting tracks it has to show you what it would delete and get your yes. While this is on it can also run your rating sync, which deletes the songs you rated 1 star when auto-delete is on, and keep a discovery track, which replaces an earlier copy of the song. At most 100 tracks in any 24 hours, for all your connected apps together. Deleted files cannot be restored.

  with the buttons **Cancel** and **Allow deleting**. Turning it off acts at once.
- **Disconnect** asks "Disconnect {app host}? It will have to be connected again from the app." and ends the app's access.
- Admins see every user's apps, with the owner's name, and can disconnect any of them. Only the owner can change an app's deletion switch.

A password change, an admin's reset of the password, or a Navidrome sign-in that links an account which had no Navidrome link disconnects every app of that user. Every new connection starts with deletion off.

Access tokens last 1 hour. Refresh tokens last 60 days from their issue, and each refresh gives a new one, so an app used at least once in 60 days stays connected. A refresh token used again more than a minute after it was replaced ends the connection.

When nothing is connected the panel says "No app is connected. Add {your address}/mcp as a custom connector in Claude to connect one."

## Deleting music

Deletion is off for every app until its owner turns on **Allow deleting music** in Settings > Users > Connected apps. With it on, an app can:

- delete songs you confirmed: `preview_delete_tracks` checks up to 25 songs and says for each whether it may be deleted and, if not, why, with a confirmation token for exactly the songs it allows; Claude shows you the preview and asks; `delete_tracks` with that token deletes them after your yes. `find_duplicates` finds songs your library holds more than once, to preview;
- replace a library copy while it downloads a better one: `ensure_in_library`'s `replace_existing` takes the token of a preview with reason `replaced`, and the previewed copies are deleted once the new copy is imported, within 24 hours;
- run your rating sync (`sync_ratings`) with its auto-delete: when you turned auto-delete on, the songs you rated 1 star are deleted;
- keep a discovery track (`promote_discovery_track`) that replaces an earlier copy of the song in your library.

The last two delete without a preview. They count against the same daily limit and appear in the deletion history under the app's name.

Without the switch, `preview_delete_tracks` and `delete_tracks` answer `permission_not_granted` with "Deleting music is turned off for this app" and the user action "Allow deleting music for this app" in "Settings > Users > Connected apps"; `ensure_in_library` refuses `replace_existing` the same way; `sync_ratings` skips auto-delete (the 6-hourly sync does it); `promote_discovery_track` replaces no earlier copy and ends `import_already_in_library`.

Some changes are refused to every app, whether or not its deletion switch is on, because they would let a later sync delete files without a preview:

- turning auto-delete on: "Only you can turn auto-delete on, in Soulful's Settings: it deletes the files of songs you rate 1 star at every rating sync";
- changing the discovery folder or the promote threshold;
- rating a song 1 star: "A connected app may not rate a song 1 star: while auto-delete is on, now or once it is turned on, the rating sync deletes the files of songs rated 1 star without a preview. Rate it 1 star yourself in Navidrome, or delete it with preview_delete_tracks and delete_tracks.";
- rating a pending discovery track at all (clearing a rating is allowed).

A pending discovery track was never in your library: `remove_discovery_track` deletes it from the discovery folder and needs no switch.

### What every deletion checks

Every deletion of a library file, from an app or from Soulbeet itself, passes these checks when it happens; after a preview, `delete_tracks` runs them all again:

1. The file is inside one of your library folders, and not possibly inside another user's.
2. It is an audio file.
3. No other listener keeps it. Another Soulbeet user linked to Navidrome keeps a track when they rated it above 1, starred it, rated its album above 1, starred its album, or a static playlist they can see holds it and is not yours. The refusal says "Another listener rated, starred or kept this track in a playlist" and never says who. When a listener's marks cannot be read, the track is kept too. A smart playlist does not keep a track. Your own playlists do not keep it either; the preview lists them so you know what changes.
4. Navidrome's average rating of the track shows no rating Soulbeet cannot read. When some rating read is above 1, every Navidrome user must be a linked Soulbeet account that sees the track, which Soulbeet can tell only when some linked account is a Navidrome admin. Otherwise the file is kept.
5. No download or import that is not finished wants the track.
6. The file is the one that was checked: a file that changed after the preview is not deleted.

Every deletion is listed by `get_deletion_history` and in the dashboard's Deletion History, with the app's name for an app's deletion, and the song leaves the folder's beets database.

A confirmation holds up to 25 songs, lasts 10 minutes and works once, for the user and the app it was made for. At most 100 tracks are deleted through apps in any 24 hours per user, all of their apps together, counted over a rolling window; past it `delete_daily_limit` says when room frees.

Limits of the checks:

- A star set by a Navidrome user who has no Soulbeet account cannot be read and does not keep a track.
- A private playlist of such a user is seen only when some linked account is a Navidrome admin.

The full list is under [Known limits](#known-limits).

## Tools

What each tool does, in the order Claude sees them. Every tool answers one JSON document, and a failure has a stable code (see [Error codes](#error-codes)).

### Status and settings

- `get_status`: what Soulbeet can do for this user now: whether slskd and beets work, the Navidrome link and scan status, the folders with free space and `in_navidrome` (`listed` when a Navidrome library Soulbeet has located on this machine holds the folder, so imports into it show up in Navidrome; `unknown` otherwise), the batches still running, whether this app may delete music, and Soulbeet's address.
- `get_settings`: the user's settings: default folder, auto-delete, metadata provider, Last.fm and ListenBrainz names (never keys), discovery settings.
- `list_folders`: the user's library folders with ids, paths, free space, whether shared, `in_navidrome`, and which is the default.

### Library

- `get_library_stats`: counts of tracks, albums and artists, the user's ratings, and the 20 most common genres.
- `find_in_library`: looks up to 100 tracks, by recording MBID, then by artist and title; matches are `mbid`, `exact` or `fuzzy`, and fuzzy ones are left for Claude to judge.
- `search_library`: free text search of the library for songs, albums or artists.
- `get_library_album`: one album with its songs in order.
- `list_starred`: the songs, albums or artists the user starred.
- `list_playlists`: the user's playlists and other users' public ones.
- `get_playlist`: one playlist with its entries in order.

### Music metadata

- `search_music`: searches MusicBrainz (or Last.fm) for recordings, releases or artists, to check that a recording exists and get its ids.
- `get_release`: a release with its tracks.
- `get_recording_releases`: a recording and the releases it appears on, to tell a studio album from a compilation or a live record.

### Downloads

- `find_sources`: searches Soulseek through slskd for a track or an album and ranks what it finds; searches run one at a time for the whole server.
- `get_sources`: reads a search `find_sources` started, a page at a time.
- `get_jobs`: shows jobs by id or by batch: state, current download, earlier attempts, import, and the calls that can be made next.
- `get_batch`: follows a batch, and can wait up to 180 seconds for a change; for a batch with a playlist, how far the playlist is.
- `list_batches`: the user's batches, newest first, so a new chat finds earlier work.
- `ensure_in_library`: the main way to get music: for up to 50 tracks or albums, says which the library has and starts a job for each other one, all in one batch, optionally filling a playlist as jobs finish.
- `retry_job`: tries a job again with the same source later, the next source, a new search, a new wait for Navidrome, or after rejecting a wrong result.
- `cancel_job`: cancels a job that is searching, waiting or downloading.
- `dismiss_job`: closes a job that will not be pursued, deleting the download kept for an import decision (never a library file).
- `download_source`: downloads one chosen candidate, for a job or as a new job.

### Import decisions

- `get_import_choices`: shows what a job waiting for an import decision kept: the files, beets' output and the matches beets weighed.
- `resolve_import`: decides that import: beets again, as-is, a candidate, a MusicBrainz id, or discard the download.

### Playlists

- `create_playlist`: creates a static playlist with the given songs in order.
- `update_playlist`: renames a playlist or changes its comment or public flag.
- `delete_playlist`: deletes one of the user's playlists; the songs stay.
- `set_playlist_tracks`: replaces every entry of a playlist, which also reorders it.
- `add_playlist_tracks`: adds songs at the end or before a position.
- `remove_playlist_tracks`: removes entries, each named by its position and song.

### Ratings and settings

- `set_rating`: sets the user's own rating of a song or album; a 1-star song rating and any rating of a pending discovery track are refused.
- `set_star`: stars or unstars a song or album.
- `update_settings`: changes the default folder, turns auto-delete off, and changes discovery's on/off, profiles, track counts, lifetimes and playlist names.

### Discovery

- `get_discovery`: the user's discovery: folder, profiles, the current run and the pending tracks.
- `run_discovery`: starts a discovery run.
- `promote_discovery_track`: moves a pending discovery track into the library; it replaces an earlier copy only with the deletion switch on.
- `remove_discovery_track`: deletes a pending discovery track from the discovery folder.
- `sync_ratings`: runs the user's rating sync now, as Soulbeet does every 6 hours.

### Deletion

- `find_duplicates`: finds songs the library holds more than once, grouped by artist and title.
- `preview_delete_tracks`: checks up to 25 songs for deletion and gives a confirmation token for the ones that may go; deletes nothing.
- `delete_tracks`: deletes the songs a confirmation token allows, from disk and from beets; there is no undo.
- `get_deletion_history`: the deletions of the user's files, newest first, with why, how and which app.

## Limits

Soulbeet's limits:

| What | Limit |
|---|---|
| Tool calls | 120 a minute per connected app |
| Downloads from apps | 6 at once per user |
| Unfinished jobs from apps | 200 per user; an item past that is refused `rate_limited` |
| Source searches from apps | one at a time on the whole server; 3 waiting per user |
| Sources tried per track | 4 by default, at most 10 (`max_attempts`) |
| `find_sources` wait | 45 seconds, then `get_sources` |
| Waits (`wait_seconds`) | up to 180 seconds per call |
| One answer | 60,000 characters; one job document 45,000 |
| `find_in_library` | 100 references per call |
| `ensure_in_library` | 50 items per call |
| `get_jobs` | 100 job ids per call |
| Pages | 25 items by default, at most 100 |
| Delete confirmation | 25 songs, valid 10 minutes, used once |
| Deletions through apps | 100 in any 24 hours per user, all apps together (`delete_daily_limit`) |
| Replacement consent (`replace_existing`) | 24 hours |
| Wait in a peer's queue | 15 minutes, then the next source |
| Failing sources | a source whose transfers failed 3 times in 24 hours on this server is flagged and never picked automatically |
| Jobs and batches | kept 30 days after they end |
| Stored searches | 24 hours |
| Request keys | 7 days |
| Access token | 1 hour |
| Refresh token | 60 days from its issue |
| Sign-in request | 10 minutes |
| Pending sign-ins | 20 per user |
| Connected apps | 20 per user; a new one ends the least recently used |
| Registered apps | 100 on the server |
| App registrations | 30 an hour on the server |
| Token requests | 30 a minute per user |
| Refreshes | 10 a minute per connection |
| Client metadata fetches | 10 a minute per user |

Claude's own limits: a tool call may take 240 seconds, and a result about 150,000 characters.

## Troubleshooting

### Claude says "Couldn't reach the MCP server"

Claude's servers could not reach your address. Check that the name has a public `A` record (see [Requirements](#requirements)), that your firewall and any WAF let `160.79.104.0/21` through, and that the address does not redirect to another host.

### Claude says "Authorization with the MCP server failed"

The sign-in started but did not finish. Two causes are common: Soulbeet's token endpoint took more than 10 seconds to answer, or the address in Claude is not the address Soulbeet derives for Claude's requests. Open [Remote access](#check-the-address) and compare.

### A connection answer with an `error` code

Claude's endpoints answer a refusal as `{"error": code, "message": text}`.

- `mcp_disabled` (404): "Claude (MCP) access is turned off: MCP=off is set", or "Claude (MCP) access is turned off: MCP is set to '{value}', which is neither on nor off". Set `MCP=on` or remove it.
- `mcp_unavailable` (503): "Claude (MCP) access stays off until the security checks in Settings pass". The answer does not say which check fails: an admin sees that in Settings > Config under Security Checks, with the texts quoted under [Requirements](#requirements). Fix what it lists.
- `address_unreadable` (400): "Cannot tell this request's address: {problem}", where the problem is one of "The request names no host (Forwarded host=, X-Forwarded-Host or Host)", "'{value}' is not a valid host", "'{value}' is not http or https", "The address must not contain a user name or password", "The address must not have a path: Soulful cannot run under a sub-path", or "PUBLIC_URL is not usable: {problem}". Fix the proxy header or `PUBLIC_URL` it names.
- `https_required` (403): "Soulful derived the address {address} for this request. Claude connects only over https: have the proxy send X-Forwarded-Proto: https (or Forwarded: proto=https), or set PUBLIC_URL." The answer also gives `derived_base`, `host_from` and `scheme_from`.
- `origin_not_allowed` (403): "Requests from {origin} are not accepted". A request to `/mcp` came with an `Origin` that is plain http and neither Soulbeet's own address nor loopback, or that is not an origin at all (`null`). Claude is not affected; this guards against DNS rebinding.
- `settings_unreadable` (503): "Soulful's database is unavailable". Check the server's disk and log.

When `PUBLIC_URL` is set but unusable, nobody can sign in, and Settings > Config says "PUBLIC_URL is not usable: {problem}. It must look like https://host or https://host:port. Nobody can sign in until it is fixed or removed."

### `/mcp` answers 401

The `WWW-Authenticate` header names `invalid_token` with one of these descriptions:

- "No access token was sent": the first request of every connection; Claude then signs in. If Claude keeps getting it, the proxy drops the `Authorization` header.
- "The access token is unknown, expired or revoked": the app was disconnected, the user's password changed, or the token expired and could not be refreshed. Connect again.
- "The access token was issued for another address of this server": the app connected at one address (for example a LAN address) and now calls another. Connect again at the public address, or set `PUBLIC_URL`.

### An error page while connecting

Soulbeet's sign-in and consent pages show these texts:

- "Soulful does not know this app. Start the connection again from the app."
- "This app asked to return to an address it did not register."
- "Your Soulful session ended. Start the connection again from the app."
- "This sign-in request expired or was already used. Start the connection again from the app." A sign-in request lasts 10 minutes.
- "Soulful did not understand the answer. Start the connection again from the app."
- "Soulful's database is unavailable. Start the connection again from the app."
- "This app sent {parameter} more than once. Start the connection again from the app."
- "This app sent a request Soulful cannot accept ({error}: {description}). Start the connection again from the app." With `invalid_target` the description is "resource must be {address}/mcp": the address in Claude differs from the address Soulbeet derives. Make them the same (see [Check the address](#check-the-address)).
- "Soulful could not use this app's client metadata at {url}: {problem}"
- "Too many app sign-ins in a minute. Try again in {n} seconds."

### OAuth errors

The registration and token endpoints answer `{"error", "error_description"}`:

- `rate_limited` at registration: "Too many app registrations on this server. Try again in {n} seconds."
- `too_many_clients`: "Soulful holds 100 registered apps, all connected. Disconnect one in Settings > Connected apps."
- `rate_limited` at the token endpoint: "Too many token requests for this Soulful account. Try again in {n} seconds." or "Too many refreshes for this connection. Try again in {n} seconds."
- `invalid_target`: "resource must be {address}/mcp", as above, or "The refresh token was issued for another address of this server".
- `invalid_grant`: "The refresh token is unknown, expired or revoked", or "The refresh token was already used; the connection is ended". Connect again.

### Tool errors

A tool that fails answers its document with `ok: false` and an `error` with these fields:

- `code`: one of the [error codes](#error-codes);
- `stage`: where it failed: `input`, `metadata`, `library_check`, `source_search`, `enqueue`, `transfer`, `file_lookup`, `import`, `index`, `playlist` or `delete`;
- `message`: what happened;
- `cause`: the other service's own answer, when there is one (see [Failure causes](#failure-causes));
- `retry`: whether and how to try again: `same_source_later` (with `retry_after_seconds` when known), `other_source`, `other_query`, `needs_decision`, `needs_user`, `not_retryable`, or `per_item` for a `partial` answer;
- `user_action`: when only a person can fix it, what to do and where (see [User actions](#user-actions));
- `facts`: what Soulbeet knew when it failed, such as the ids involved;
- `tried`: earlier attempts and how each failed, when there were any;
- `next`: the calls that can still be tried.

The ones you are most likely to see:

- `rate_limited`: "Too many tool calls from this app: at most 120 a minute. Try again in {n} seconds." Claude waits and tries again.
- `permission_not_granted`: deletion is off for the app ("Deleting music is turned off for this app"), or the change is one no app may make (see [Deleting music](#deleting-music)).
- `delete_daily_limit`: "At most 100 tracks a day can be deleted through connected apps, and {used} were deleted in the last 24 hours. {n} more can be deleted from {time}."
- `expired_handle` for a confirmation: "This confirmation token was already used" or "This confirmation token expired at {time}; preview the songs again". Claude makes a new preview.

## Error codes

| Code | Stage | What it means | What to do |
|---|---|---|---|
| `invalid_input` | `input` | An argument is missing, has the wrong type or range, or is one the tool does not take; `facts.expected` names what it takes. Also a request the state does not allow, such as running discovery while it is off. | Claude corrects the call. |
| `unknown_handle` | `input` | An id that names nothing this user may use: a job, batch, search, candidate, folder, song, album, discovery track, confirmation token or connected app. Another user's id reads the same. | Get the id again from the tool that lists it. |
| `expired_handle` | `input`, `import`, `delete` | The id was valid but is no longer usable: a search older than 24 hours, a confirmation token used or older than 10 minutes, a replacement consent past its 24 hours, a discovery track no longer pending. | Start again from the step that gave it: a new search or a new preview. |
| `not_owner` | `input` | The discovery track belongs to another user. | Only its owner can keep or remove it. |
| `permission_not_granted` | `input`, `delete` | The app may not do this: deletion is off for it (user action `enable_deletion`), or no app may make the change: auto-delete on (`enable_auto_delete`), the discovery folder or promote threshold (`change_discovery_settings`), a 1-star song rating or a rating of a pending discovery track (`rate_in_navidrome`). | Do what the user action says: tick Allow deleting music in Settings > Users > Connected apps, change the setting in Settings > Library, or rate in Navidrome. |
| `folder_required` | `input` | There is no folder to download into: no `folder_id` and no default folder, no library folder at all, or no discovery folder. User action `add_folder` when one is missing. | Add a folder or choose a default in Settings > Library, or a discovery folder in Settings > Library > Discovery. |
| `rate_limited` | `input`, `source_search` | Too many tool calls from this app, 3 source searches already waiting for this user, or 200 unfinished app jobs for this user. | Wait; `retry_after_seconds` says how long where it is known. |
| `metadata_unavailable` | `metadata` | MusicBrainz or Last.fm did not answer or answered with an error. | Try again later. |
| `metadata_not_found` | `metadata` | The provider has no such recording, release or artist. | Search again with other words or another id. |
| `metadata_rate_limited` | `metadata` | MusicBrainz or Last.fm refused for too many requests. | Try again later. |
| `provider_not_configured` | `metadata` | Last.fm refused the API key or none is saved; a `lastfm:` id needs one. User action `configure_provider`. | Save a valid Last.fm API key in Settings > Library, or use MusicBrainz. |
| `navidrome_unreachable` | `library_check`, `index`, `playlist`, `delete` | Navidrome did not answer, answered an HTTP error (often a proxy or a wrong `NAVIDROME_URL`), or answered something that is not its API. From `find_duplicates`: Navidrome's pages held fewer songs than it counts. | Check that Navidrome runs and `NAVIDROME_URL` is right; try again. |
| `navidrome_auth_failed` | `library_check`, `index`, `playlist`, `delete` | Navidrome refused the stored Navidrome password. User action `relink_navidrome`. | Sign in to Soulbeet again with your current Navidrome password. |
| `navidrome_not_linked` | `library_check`, `index`, `playlist`, `delete` | The account is not linked to Navidrome or its stored password cannot be read (user action `relink_navidrome`), or `NAVIDROME_URL` is not set (user action `configure_navidrome`). | Sign in to Soulbeet with your Navidrome account, or set `NAVIDROME_URL`. |
| `navidrome_circuit_open` | `library_check`, `index`, `playlist`, `delete` | Navidrome failed 5 times in a row, so Soulbeet sends it nothing for 60 seconds. | Wait a minute; check Navidrome. |
| `navidrome_refused` | `library_check`, `index`, `playlist`, `delete` | Navidrome answered with an error other than a refused login, for example an account that may not do what was asked. | Read `cause`; trying again gives the same answer. |
| `backend_not_configured` | `source_search`, `enqueue`, `transfer` | The slskd URL or API key is missing, does not parse, or slskd refused the key. User action `configure_backend`. | An admin sets the slskd URL and API key in Settings > Config. |
| `backend_unreachable` | `source_search`, `enqueue`, `transfer` | slskd did not answer, is not connected to the Soulseek network, or is not logged in (then user action `configure_backend`: check slskd's Soulseek user name and password). | Check slskd; try again later. |
| `settings_unreadable` | any | Soulbeet could not read its own database. | Check the server's disk and log; try again later. |
| `backend_response_unreadable` | `source_search`, `enqueue`, `transfer`, `metadata` | slskd, or a metadata provider, answered something Soulbeet cannot read. For slskd, user action `update_soulful_or_backend`. | Update Soulbeet or slskd so that their versions match. |
| `search_rate_limited` | `source_search` | slskd refused the search for too many requests. | Try again later. |
| `search_no_results` | `source_search` | Nothing on the Soulseek network matched. | Search with other words. |
| `search_all_filtered` | `source_search` | Results came back, but each one scored too low or was cut by the filters; the facts count each. | Loosen the filters (for example `lossless_only`) or change the query. |
| `search_timeout` | `source_search` | The search did not end in time. | Try again later. |
| `search_query_rejected` | `source_search` | slskd refused the search text as unusable, for example one of exclusion terms only. | Search with other words. |
| `search_already_running` | `source_search` | slskd already runs a search under the id asked for. | Try again later. |
| `search_lost` | `source_search` | slskd no longer has the search Soulbeet was reading. | Run the search again. |
| `peer_offline` | `enqueue`, `transfer` | The source is offline. | Another source, or the same one later. |
| `already_running` | `input`, `enqueue` | The same work is already going: a job for this recording, a call with this `request_key`, a rating sync or discovery run of this user, or a download of this user taking the same file into this folder. | Follow the one that runs. |
| `already_in_flight_other_user` | `enqueue` | Another user's download takes the same file. | Another source. |
| `already_in_flight_other_folder` | `enqueue` | This user already downloads the same file into another folder. | Wait for it, or another source. |
| `enqueue_failed` | `enqueue` | slskd did not take the download: it could not reach the source, its blacklist holds the source, or it refused the request. | Another source, or as `retry` says. |
| `slskd_too_old` | `enqueue` | slskd is older than 0.26. User action `upgrade_backend`. | Upgrade slskd to 0.26 or newer. |
| `transfer_connection_failed` | `transfer` | The connection to the source failed or dropped, or slskd restarted during the transfer. | Another source; after a slskd restart, the same one later. |
| `transfer_size_mismatch` | `transfer` | The source's file has another size than it announced. | Another source. |
| `transfer_rejected_not_shared` | `transfer` | The source says the file is not shared. | Another source. |
| `transfer_rejected_peer_busy` | `transfer` | The source's queue is full, or it hit a daily or weekly quota. | The same source later for a full queue, another source for a quota. |
| `transfer_rejected_failure_limit` | `transfer` | The source refuses after too many failed transfers today or this week. | Another source. |
| `transfer_rejected_other` | `transfer` | The source refused for another reason, given in `cause`. | Another source. |
| `transfer_timed_out` | `transfer` | The transfer timed out in slskd, or ran past Soulbeet's time for one track. | Another source. |
| `transfer_aborted` | `transfer` | slskd aborted the transfer. | Another source. |
| `transfer_cancelled` | `transfer`, `import` | The transfer was cancelled, in Soulbeet or in slskd. | Nothing, or retry the job. |
| `transfer_never_appeared` | `transfer` | slskd took the request but never listed the transfer. | Another source. |
| `transfer_disappeared` | `transfer` | The transfer vanished from slskd's list. | The same source later. |
| `transfer_stuck_in_remote_queue` | `transfer` | The transfer waited 15 minutes in the source's queue. | Another source. |
| `transfer_list_unreadable` | `transfer` | slskd's list of downloads could not be read. | Check slskd; update Soulbeet or slskd if their versions do not match. |
| `file_not_found_after_download` | `file_lookup` | slskd finished, but Soulbeet cannot find the file where it looks. User action `fix_download_path` when the file is not on disk. | Mount slskd's download folder in Soulbeet at `DOWNLOAD_PATH`. |
| `import_no_match` | `import` | beets found no close enough match. | Claude reads `get_import_choices` and decides with `resolve_import`, or you decide in the web page. |
| `import_already_in_library` | `import`, `index` | The library already holds this recording; the facts give its song ids. | Nothing. To replace it, preview with reason `replaced` and pass `replace_existing`. |
| `import_nothing_to_import` | `import` | beets found nothing to import in the downloaded files. | Another source. |
| `import_lookup_failed` | `import` | beets could not look the files up. | Try again later. |
| `import_beets_failed` | `import` | beets failed; `cause` holds the end of its output. | Try again later; read beets' output. |
| `import_target_unwritable` | `import` | beets cannot write into the library folder. | Fix the folder's permissions. |
| `import_disk_full` | `import` | The disk beets writes to is full. User action `free_disk_space`. | Free disk space, then retry. |
| `import_idle_timeout` | `import` | beets printed nothing for too long and was stopped. | Try again later. |
| `import_total_timeout` | `import` | beets ran past its time limit and was stopped. | Try again later. |
| `importer_broken` | `import` | beets is not set up so that Soulbeet can run it. User action `fix_importer`. | Fix the beets setup; Settings > Config shows the problem. |
| `import_as_is_missing_tags` | `import` | An as-is import was refused: some files lack artist and title tags (and album, in album mode). | Choose a match instead. |
| `import_choice_invalid` | `input`, `import` | The chosen candidate is not one beets found, or a MusicBrainz link names the wrong kind of entry. | Choose again. |
| `import_files_gone` | `import` | The downloaded files kept for the decision were deleted. | Dismiss the job, or download again. |
| `import_partial` | none | Not sent by this version of Soulbeet. | Nothing. |
| `index_not_found_after_scan` | `index` | Navidrome scanned after the import and does not list the file. | Check that a Navidrome library holds the folder (`in_navidrome`), then retry with `index_only`. |
| `index_no_scan_possible` | `index` | Navidrome has not scanned for 10 minutes since the import, and this account cannot start a scan. User action `enable_indexing`. | Make the Navidrome account an admin, turn on Navidrome's file watcher, or set a scan schedule. |
| `scan_failed` | `index` | Navidrome's scan failed. | Try again later. |
| `playlist_not_found` | `playlist` | No such playlist, or another user's private one. | List the playlists again. |
| `playlist_not_owner` | `playlist` | Only the owner of the playlist can change it. | Make a copy of your own. |
| `playlist_name_taken` | `playlist` | You already have a playlist with this name. | Another name, or `allow_duplicate_name`. |
| `playlist_tracks_not_written` | `playlist` | Navidrome does not list the playlist's songs as they were written. | Read the playlist and try again. |
| `playlist_not_written` | `playlist` | Navidrome answered a change of name, comment, public flag or a delete, but does not show it. | Read the playlist and try again. |
| `playlist_changed` | `playlist`, `input` | A position given no longer holds the song given. | Read the playlist again. |
| `playlist_not_editable` | `playlist` | A smart or file-synced playlist, whose tracks Navidrome sets itself. | Use a static copy. |
| `delete_path_unresolved` | `delete`, `library_check` | Soulbeet cannot find the song's file where Navidrome's library puts it. For `set_rating` while you have pending discovery tracks, user action `configure_navidrome`. | Make Navidrome's music folder readable by Soulbeet at Navidrome's path, or set `NAVIDROME_MUSIC_PATH`. |
| `delete_outside_library` | `delete` | The file is not inside one of your library folders, or may be inside another user's. | Nothing; only files in your folders can be deleted. |
| `delete_not_a_track` | `delete`, `library_check` | The file is not an audio file. | Nothing. |
| `delete_blocked_by_other_user` | `delete` | Another listener rated, starred or kept this track in a playlist. | Nothing; Soulbeet does not say who. |
| `delete_veto_unknown` | `delete` | Soulbeet cannot rule out a mark it cannot read: Navidrome is older than 0.60 or its version cannot be read, a listener's ratings could not be read, a Navidrome user has no Soulbeet account, or a linked account cannot see the track. User action `relink_navidrome` or `configure_navidrome` where one helps. | Do what the user action says: have every Navidrome user sign in to Soulbeet once, sign in once as a Navidrome admin, or give every linked account access to the track's library. |
| `delete_in_use` | `delete` | A download or import that is not finished wants this track. | Wait until it ends. |
| `delete_changed_since_preview` | `delete` | The file changed after the preview, or was not in it. | Preview again. |
| `delete_failed` | `delete`, `import` | The system refused to remove a file every check allowed; the message gives its reason. | Fix the file's permissions. |
| `delete_daily_limit` | `delete` | 100 tracks were deleted through apps in the last 24 hours, or more than 100 were asked at once. | Wait until the time the message names. |
| `server_restarted` | `source_search`, `transfer`, `import` | Soulbeet restarted while the search, download or import ran. | Retry the job. |
| `partial` | any | Some items of a call failed and the others were done; each failed item has its own error. | Follow each failed item's error. |
| `wrong_job_state` | `input` | The job's state does not allow the change asked for. | Use a call the job's `next` offers. |
| `sources_exhausted` | `source_search` | The job's search has no source left that it has not tried. | A new search. |
| `result_rejected` | `import` | You said a done job's file is not the recording wanted; the job reopens without that source. | The job tries another source. |
| `search_stopped` | `source_search` | The search was stopped because its job was cancelled. | Nothing. |

## User actions

A failure only a person can fix carries a `user_action` with a code, what to do and where. The texts are Soulbeet's own.

| Code | What | Where |
|---|---|---|
| `configure_backend` | "Set the slskd URL and API key", or "Check the Soulseek username and password slskd signs in with" | "Settings > Config", or "slskd" |
| `upgrade_backend` | "Upgrade slskd to 0.26 or newer" | "slskd" |
| `update_soulful_or_backend` | "Update Soulful or slskd so that Soulful can read slskd's answers again" | "Soulful and slskd" |
| `fix_importer` | "Fix the beets setup: {problem}" | "Settings > Config" |
| `fix_download_path` | "Make the folder the download backend saves into readable by Soulful at {DOWNLOAD_PATH}" | "DOWNLOAD_PATH and the volume mounted there" |
| `free_disk_space` | "Free disk space: {message}" | "The disk that holds the path in the message" |
| `relink_navidrome` | "Sign in to Soulful again with your current Navidrome password", "Sign in to Soulful once as a Navidrome admin, so Soulful can list Navidrome's users", or "Have every Navidrome user sign in to Soulful once, so Soulful can read their ratings" | "Soulful sign-in" |
| `configure_navidrome` | "Set NAVIDROME_URL to the address of your Navidrome server", "Make Navidrome's music folder readable by Soulful at Navidrome's library path, or set NAVIDROME_MUSIC_PATH to where Soulful sees Navidrome's default library", or "Give every Navidrome account linked to Soulful access to this track's library, so Soulful can read their ratings of it" | "Soulful's environment", or "Navidrome's user settings" |
| `configure_provider` | "Set a valid Last.fm API key" | "Settings > Library" |
| `enable_deletion` | "Allow deleting music for this app" | "Settings > Users > Connected apps" |
| `enable_indexing` | "Make this Navidrome account an admin, enable Navidrome's file watcher, or set a scan schedule" | "Navidrome" |
| `add_folder` | "Add a library folder", or "Choose a discovery folder" | "Settings > Library", or "Settings > Library > Discovery" |
| `rate_in_navidrome` | "Rate the song 1 star yourself in Navidrome", or "Rate the song yourself in Navidrome" | "Navidrome" |
| `enable_auto_delete` | "Turn on auto-delete yourself" | "Soulful Settings > Library" |
| `change_discovery_settings` | "Change the discovery promote threshold yourself", or "Change the discovery folder yourself" | "Soulful Settings > Library > Discovery" |

## Failure causes

A failure's `cause.service` names whose answer `cause.raw` holds: `slskd`, `beets`, `navidrome`, `musicbrainz`, `lastfm`, `listenbrainz`, `filesystem`, `database` (Soulbeet's own settings database), or `soulful` (Soulbeet's own words about what it found wrong, or lines it could not read from a service). `cause.status` is the service's status code when it gave one.

## Known limits

Deletion and the checks it passes:

1. Deletion needs Navidrome 0.60 or newer. Below 0.60, or when Navidrome reports no version Soulbeet can read, every deletion and replacement is refused and the file is kept.
2. The song reads need Navidrome 0.55 or newer. An older answer does not parse, the read fails and the file is kept.
3. Stars of Soulbeet users who never linked Navidrome, and of Navidrome users with no Soulbeet account, cannot be read, so they do not keep a track. Their ratings count only through Navidrome's average (check 4). A private playlist of a Navidrome user with no Soulbeet account is seen only when some linked account is a Navidrome admin. A smart playlist never keeps a song, even when its rules name that one track.
4. When some rating Soulbeet reads for a track is above 1 (yours included), the file is kept unless a linked account is a Navidrome admin and every Navidrome user is a linked account that sees the track. So replacing a well rated track keeps both copies on a server where some Navidrome user never signed in to Soulbeet, or where no admin did. Navidrome rounds the average to 2 decimals: with 200 or more ratings of 1, one hidden rating of 2 rounds away.
5. A linked listener whose Navidrome account cannot see a track (per-user libraries) keeps it whenever some rating read is above 1, since a rating they gave before losing access stays in the average and cannot be read. This holds even when they never rated it.
6. A rating or star set between the last check and the removal is not seen: one round trip for a single deletion, longer across the items of one replaced album.
7. A file renamed onto the checked name in the same directory between the last check and the removal is removed instead.
8. Soulbeet finds Navidrome's songs on disk through `NAVIDROME_MUSIC_PATH` for the default library, else through library roots it learns from imported files whose modification time matches what Navidrome recorded. Mounts where Navidrome and Soulbeet read different modification times never learn a root, so deletions there are kept unless the paths are equal or `NAVIDROME_MUSIC_PATH` covers the default library. A root once learned is kept while it is a directory, and a missing file under it is refused ("No file at {path}"). With no root at all, a copy with the same size, path ending and modification time can teach a wrong root until an import shows the real one.
9. A second song of the file's size in a Navidrome library Soulbeet cannot see (a partial mount) keeps the file.
10. Discovery imports keep beets' own duplicate handling, so beets may remove an earlier discovery item without the deepest folder check (another user's folder registered inside a shared `Discovery/<profile>` folder).
11. A replacement deletes the earlier tracks, not the earlier album's cover file; with fetchart the old cover stays beside the new one.
12. A removal that stopped before it reported, whose path then holds nothing, is recorded as a deletion and its beets item forgotten, also when something else removed the file meanwhile. When the path holds another file, the deletion is recorded and beets' item at that path is left as it is; the server log says so.
13. Any answer of Navidrome's user list other than a list or a refusal for a non-admin counts as unreadable: the file is kept and the refusal says to try later.
14. A stored library root that is no longer a directory, while Navidrome's own path exists on this machine, keeps deletions under that library refused ("No file at {path}") until an import shows a root again.

Downloads and imports:

15. A job whose failure the database refused 7 times waits with no runner until Soulbeet starts again, which resumes or ends it. Until then `next` offers no wait on it.
16. `list_batches` reads every batch of the user to filter by label; fine at a household's size.
17. A job document is kept within 45,000 characters for the cases Soulbeet knows, not proven for every input.
18. A kept import stored before its files were recorded has none: `dismiss_job` and `resolve_import` with `discard` end it and delete nothing.
19. A kept import stays on disk until someone decides it, as in the web page. A file the system refuses to delete stays too and is listed in `files_left`.
20. beets reports every file of a run; a kept import keeps the first 200 and counts the rest in `files_omitted`.

Ratings, discovery and playlists:

21. A connected app's rating is refused when it could lead to a deletion later, whatever its switch and your settings: a 1-star song rating, and any rating of a pending discovery track other than clearing it. No app can change the discovery folder or the promote threshold. While you have pending discovery tracks, a rating also needs Soulbeet to find the song's file, and is refused when it cannot (the music folder or `NAVIDROME_MUSIC_PATH` is wrong).
22. Navidrome's scanner can give a newly staged discovery file the id and rating of a missing library song. An app's earlier 5-star rating of that song then reads as the track's, and the next sync, yours or the 6-hourly one, promotes the track and replaces any earlier copy as Soulbeet's own deletion, outside the app's daily limit. This is rare and not closed.
23. A batch whose playlist could not begin, and whose removal then failed, shows in `list_batches` until it is pruned, 30 days after it was made.

The delete tools:

24. A song added or removed while `find_duplicates` reads, or with Navidrome's `RecentlyAddedByModTime` a file that changes, can shift Navidrome's pages, and the search then fails with `navidrome_unreachable` ("Navidrome counts {total} songs, and its pages held {read}. Search again."). A song removed while it reads can let another pass unseen, so search again after deleting.
25. A deletion whose task stopped before it reported is recorded outside the day's count, so an app's count can reach 101 once.
26. `replace_existing` replaces only for track items whose previewed file is inside the folder the jobs import into. A previewed copy beets finds no duplicate of stays and is listed in `not_replaced`, as is every copy a job did not replace within its 24 hours.
27. `preview_delete_tracks`, `delete_tracks` and `get_deletion_history` show the absolute paths of the files, which are in the user's own folders.

Proxies and Claude:

28. Behind Cloudflare, clients on protocol 2026-07-28 get no byte until a tool answers, so a wait longer than about 120 seconds can end in Cloudflare's 524 after 125 seconds while the work goes on.
29. `get_tag_tracks` (tracks of a genre or tag from Last.fm or ListenBrainz) is not built.

## Security

What exposing Soulbeet for Claude adds:

- The OAuth endpoints and `/mcp` answer only while the Security Checks pass, and only over https or on a loopback address. `MCP=off` turns them all off.
- Access and refresh tokens are opaque, stored only as hashes, and bound to the address they were issued for.
- Each token belongs to one user, whose account is loaded again on every call: a disconnected app, a changed password or a deleted account stops it at once.
- No tool takes a file path or returns a credential. A tool shows the user's own data, and other users' public playlists as Navidrome lists them; a deletion refusal never names the listener who kept the track.
- A deletion needs the owner's switch, a preview, and a second call with its single-use token, bound to that user and that app.
- No limit is keyed by the client's IP address. Put any IP rules in the proxy.
- Keep the admin password and `SECRET_KEY` safe, and back up `secret.key` with the database.

## Upgrading

The release that adds Claude (MCP) access changes these on the first start:

- `MCP` is on unless you set it off, but the endpoints stay closed until the Security Checks in Settings > Config pass.
- A `SECRET_KEY` under 32 bytes is no longer used, nor the key built into earlier versions for installs without `SECRET_KEY`: Soulbeet signs sessions with the key it generates in `secret.key`. Everyone is signed out once; stored Navidrome passwords are stored again under the new key on that start.
- A browser tab left open across the upgrade cannot read a Deletion History row of reason `cleanup` until it reloads.
- The checks' messages no longer name file paths, in Settings > Library ("Kept by auto-delete") and in import notes.
- Turning on **Allow deleting music** for an app asks first, with the text quoted under [Connected apps](#connected-apps).
