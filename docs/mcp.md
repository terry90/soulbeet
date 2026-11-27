# Soulbeet's MCP server

Soulbeet is an MCP server, so any MCP client that supports remote servers with OAuth can use it: Claude, ChatGPT, Cursor or an agent of your own. You add your Soulbeet to your AI assistant as a remote MCP server and ask for music in plain words: "find these eight carols, get the ones I don't have and make a playlist of them". The assistant then checks your library, searches Soulseek, downloads, imports with beets and fills the Navidrome playlist through Soulbeet's tools. Soulbeet has no list of allowed clients; it was tested against claude.ai.

This page is for the person who runs Soulbeet. It covers what your server needs, how to put it behind a reverse proxy, how each person connects, what an MCP client may and may not do, and what each error means.

## What it is

Soulbeet answers MCP clients at `https://{your address}/mcp`, for whatever address you serve it at. There is no fixed host name to configure.

Each person connects their own Soulbeet account, and the client then acts as that person only: their folders, downloads, playlists, ratings and settings.

Through Soulbeet, an MCP client can:

- read Soulbeet's status: whether slskd, beets and Navidrome work, your folders and their free space;
- read and browse your Navidrome library: search it, filter/sort songs or albums by folder, genre, rating, format, bitrate, year or missing state, find tracks by MusicBrainz id or artist and title, list artists' albums, starred items and playlists, and trigger quick or full Navidrome scans;
- summarize your listening history, preview recommendations read-only without downloading, audit library health (untracked files, missing tracks, split albums, missing MBIDs, art or lyrics), and fetch missing cover art and synced `.lrc` lyrics;
- look up recordings and releases on MusicBrainz;
- search Soulseek for sources and download a chosen one;
- get a list of tracks or albums into your library in one batch, and follow it;
- decide imports that beets skipped;
- create, rename and edit your static and smart playlists, rate and star songs and albums;
- run discovery, keep or remove discovery tracks, and run your rating sync;
- change some of your settings;
- find duplicates, inspect library tracks, move tracks/albums between folders, re-tag or re-match via beets, and delete music; moving, re-tagging and deleting only work once you switch file management on for that app (see [Deleting music](#deleting-music)).

Long work goes on in Soulbeet after the chat ends: a batch of downloads keeps running, imports finish, and a playlist asked for with the batch keeps filling. In a new chat the assistant finds earlier work with `list_batches`.

## Requirements

- **A public name, for hosted clients.** Hosted assistants such as claude.ai connect from their vendor's servers, not from your browser, so the name must resolve to public addresses. For claude.ai the name needs an `A` record, and every address it resolves to must be globally routable: not `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `100.64.0.0/10`, loopback or link-local. A name with only an `AAAA` record does not work. A client that runs on your own machine or on your LAN only needs to reach the https address of Soulbeet. Soulbeet does not check that its own name is public: it requires https, or a loopback address.
- **https with a certificate the client trusts.** Soulbeet refuses the OAuth and `/mcp` endpoints over plain http, except on a loopback address for local tests.
- **Soulbeet at the root of that name.** Soulbeet cannot run under a sub-path: `https://music.example.com` works, `https://example.com/music` does not.
- **The Security Checks in Settings > Config pass.** Until they do, the OAuth and `/mcp` endpoints answer `mcp_unavailable`. The two checks are:
  - `SECRET_KEY` is unset (Soulbeet then generates a key) or at least 32 bytes. A shorter one is not used at all: Soulbeet signs sessions with the key in `secret.key` instead. Settings shows: "SECRET_KEY is shorter than 32 bytes, so Soulbeet does not use it and signs sessions with the key stored in secret.key next to the database. Remove SECRET_KEY, or set it to a random value of at least 32 bytes, for example the output of `openssl rand -hex 32`. Setting a new SECRET_KEY ends every session and makes the stored Navidrome passwords unreadable: each user enters theirs again once, by signing in with it or under Connect Navidrome in Settings > Users."
  - The seeded `admin` account no longer has the password `admin`. Signing in with that password asks for a new one before anything else. Settings shows: "The seeded admin account still has the password 'admin'. Change it in Settings > Users."

Two environment variables concern MCP access:

- `MCP`: `on` or `off`, in any case, for the OAuth and `/mcp` endpoints MCP clients use; any other value turns them off and logs a warning. Unset is `on`.
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
- **Let one answer take 60 seconds.** A tool call can wait up to 30 seconds on the server and then read for up to 20, so an answer comes within 50 seconds. That fits the 60 second read timeout most proxies have by default. Clients on protocol 2025-11-25 or older (claude.ai is one, per Claude's documentation) get the response headers at once and a keep-alive every 15 seconds while a tool waits. Clients on protocol 2026-07-28 get no byte until the tool answers.
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

- Cloudflare answers 524 when the origin sends nothing for 125 seconds. Soulbeet answers a tool call within about 50 seconds, so this limit is not reached.
- WAF and bot rules must let your MCP client's addresses through. For claude.ai these are Anthropic's, `160.79.104.0/21`.

## Check the address

Admins find **Remote access** in Settings > Config. It shows:

- the address Soulbeet derived for the page's own request ("Soulbeet sees this page's request at {address}"); open Settings at your public address, so the request passes through the same proxy as the MCP clients' requests;
- where the host and the scheme came from: `forwarded`, `x_forwarded_host`, `host`, `authority`, `public_url` or `missing` for the host, and `forwarded`, `x_forwarded_proto`, `local_host_is_http`, `named_host_is_https`, `public_url` or `unread` for the scheme;
- advice lines, which compare that address with the one your browser is on:
  - "MCP access is turned off: {why}."
  - "MCP access stays off until the security checks in Settings pass."
  - "PUBLIC_URL pins the address to {address}; request headers are not read."
  - "Your browser is on {browser host}, but the request reached Soulbeet naming {host}. Have the proxy send X-Forwarded-Host (or Forwarded host=) with the public name."
  - "Your browser is on {browser host}, but the proxy names {host} in X-Forwarded-Host. Check the proxy's forwarded host." (or "in Forwarded")
  - "Your browser uses https, but Soulbeet sees http: have the proxy send X-Forwarded-Proto: https (or Forwarded proto=https)."
  - "MCP clients cannot use this address: it is plain http."
  - "Soulbeet cannot tell this request's address: {problem}"
  - "This address works for MCP clients: add {address}/mcp as a remote MCP server in your AI assistant."
- **Latest connection requests**: requests to `/.well-known/`, `/oauth/` and `/mcp`, with the time, the address derived, where it came from, the `Host`, `X-Forwarded-Host`, `X-Forwarded-Proto` and `Forwarded` headers as received, and the status with its refusal code. Soulbeet keeps the latest 50 of each of three kinds: refusals that keep MCP clients out (`origin_not_allowed`, `https_required`, `mcp_unavailable`, `mcp_disabled`, `address_unreadable`), other refused or failed requests, and answered ones. The list is in memory and starts empty when Soulbeet starts.

## Connect a client

Any client that supports remote MCP servers over streamable HTTP with OAuth adds `{address}/mcp`. It can register itself with dynamic client registration, or identify itself with a client ID metadata document (an https URL that serves the client's name and redirect URIs; Soulbeet fetches it, so it must be on a public address). Redirect URIs must be https, or http on a loopback address. Soulbeet shows the consent page described below, and you click **Allow**. The steps below are the worked example for claude.ai.

### claude.ai and the phone app

1. On claude.ai, open **Customize > Connectors**, then **Add > Add custom connector**.
2. Give it a name, for example "Soulbeet", and the address `https://music.example.com/mcp`.
3. Keep the options marked **Detected**. Either OAuth client choice works: "Use Claude's published identity" and "Register automatically" are both supported.
4. Click **Add**, then **Connect**. claude.ai opens Soulbeet in your browser.
5. If you are not signed in, Soulbeet shows its sign-in page (`/login/return?return_to=...`). Sign in with your Soulbeet account, with your Navidrome account when Soulbeet uses Navidrome sign-in, or with the **Sign in with ...** button when an admin set up single sign-on. If a single sign-on fails, the page that says why keeps the request, so signing in from there still leads to the next step.
6. Soulbeet then asks:

   > **Connect claude.ai to Soulbeet**
   >
   > claude.ai wants to use Soulbeet as {your user name}.
   >
   > Once allowed, it can see your library, downloads and settings, search for music, download into your folders, and manage your playlists and ratings. It cannot delete, move or re-tag library files unless you turn that on for it in Settings > Users > Connected apps.
   >
   > When you answer, Soulbeet sends you back to claude.ai.

   When the app names itself, the page adds "It calls itself "{name}"." For an app whose every return address is on your own computer, it adds "This app runs on a computer (it returns to {host}). Allow it only if you started it yourself." Click **Allow**. **Deny** sends you back with the connection refused.

On Team and Enterprise plans an Owner adds the connector in **Organization settings > Connectors**, and then each member connects with their own Soulbeet account. On the Free plan you can have one custom connector.

In the phone app: connectors connected on the web work in the mobile app. Turn one on for a chat with **+ > Connectors**. claude.ai asks before it calls a tool (**Allow once** or **Always allow**), and you can set what it may call without asking on the connector's page.

## Accounts

Every person connects their own Soulbeet account, and an app connected by one person acts as that person. By default, the client sees only their own folders, downloads, playlists, ratings and settings, and reads Navidrome as their Navidrome account sees it. No tool manages users, folders or the slskd settings, so an admin's client cannot change those either.

### Multi-account admin library access

When a Soulbeet **Admin** wants an assistant to organize libraries across household accounts (for example, moving tracks from another family member's private folder into a shared folder), they can turn on **Admin library access (all users' folders)** for their own connected app in **Settings > Users > Connected apps**. It starts off for every new connection and is only available on grants owned by an Admin account.

With **Admin library access (all users' folders)** enabled:

- `get_status` reports `"allow_admin": true`.
- `list_folders` defaults to `scope: "all"` and returns every registered library folder across all users with `owner`, `shared`, and `is_own` fields (pass `scope: "own"` to list only your own folders).
- `find_in_library`, `browse_library`, `get_playlist`, `find_duplicates`, and `inspect_tracks` resolve track folders across all registered users and include `owner` and `is_own` metadata.
- `move_tracks` can move tracks from or into any registered user's folder (for example, from a private folder into a shared folder, which expands visibility). Moving requires **Allow managing library files (delete, move, re-tag)** as well, and all safety guards remain enforced (Discovery staging protection, active download checks, and the multi-user visibility-shrinking veto when moving out of a shared or multi-user-visible folder into a non-shared folder). If `target_folder_id` is omitted, `move_tracks` returns `folder_required` with all accessible folders so the assistant can ask which folder to use.

How accounts appear depends on `REGISTRATION` (see [configuration.md](configuration.md)): with Navidrome sign-in (`navidrome`, the default when `NAVIDROME_URL` is set), a Navidrome user's Soulbeet account is created the first time they sign in, which can be the sign-in of the connection itself. With `admin`, an admin creates the accounts; with `open`, anyone can register.

A user whose account is not linked to Navidrome can still use the tools that do not read Navidrome; those that do answer `navidrome_not_linked`.

## Navidrome scans

After an import Soulbeet waits until Navidrome lists the new files, and asks Navidrome to scan when it does not yet. Navidrome lets only its admins start a scan, and a scan covers every library for every user. So when your own Navidrome account is not an admin, Soulbeet starts the scan with the account of another Soulbeet user linked to the same Navidrome. Every linked account signs in to the one `NAVIDROME_URL`. Whether your own account is an admin Soulbeet reads with your account. Another account is only ever sent the scan request: Soulbeet learns from Navidrome's answer whether that account may start a scan, reads nothing with it, and shows you nothing of it. A refused request changes nothing in Navidrome. The wait still reads the scan with your own account.

Soulbeet remembers what Navidrome said of each account for 15 minutes, so it does not ask before every import. It tries first the accounts Navidrome last let scan, then those it knows nothing of, and passes over those Navidrome refused, including one whose stored password it refuses. A user made an admin in Navidrome is used within 15 minutes, a new Navidrome password or sign-in makes Soulbeet learn again at once, and an account deleted in Soulbeet is no longer used. When no linked account may start a scan, Soulbeet sends no scan request and logs no warning: new tracks appear at Navidrome's next own scan, by its file watcher or its scan schedule. `get_status` says which case holds, from what Soulbeet already knows and without asking any other account: `can_trigger_scan` is `true` or `false`, or `null` when your account is not an admin and nothing is known yet of the others (after a restart, or 15 minutes after the last import), and `scan_note` says why: "Your Navidrome account is an admin, ...", "Your Navidrome account is not an admin, so Soulbeet starts the Navidrome scan after your imports with another linked account that is.", "Your Navidrome account is not an admin. Soulbeet learns at your next import whether another linked account may start the Navidrome scan; ...", or "No Navidrome account linked to Soulbeet may start a scan, so new tracks appear at Navidrome's next own scan (its file watcher or scan schedule). ...". To let Soulbeet scan, sign in to Soulbeet once as a Navidrome admin.

## Connected apps

**Settings > Users > Connected apps** lists the apps you connected. Each row shows the app's host (for example `claude.ai`), "It calls itself "{name}"." when the app gave a name, "Last used {date}" or "Never used", the **Allow managing library files (delete, move, re-tag)** box (only on your own apps) and **Disconnect**.

- **Allow managing library files (delete, move, re-tag)** is off for every new connection. Turning it on asks first:

  > Let {app host} manage library files (delete, move between folders, and re-tag)? Before deleting, moving or re-tagging tracks it has to ask you first. While this is on it can also run your rating sync, which deletes the songs you rated 1 star when auto-delete is on, and keep a discovery track, which replaces an earlier copy of the song. Deletions are limited to at most 100 tracks in any 24 hours, for all your connected apps together. Deleted files cannot be restored.

  with the buttons **Cancel** and **Allow managing files**. Turning it off acts at once.
- **Admin library access (all users' folders)** is shown on an admin's own apps and starts off for every new connection. Turning it on asks for confirmation before allowing that app to list and inspect all users' registered folders and perform cross-user folder moves (with file management also enabled). Turning it off acts at once.
- **Disconnect** asks "Disconnect {app host}? It will have to be connected again from the app." and ends the app's access.
- Admins see every user's apps, with the owner's name, and can disconnect any of them. Only the owner can turn on an app's file-management or admin-access switches.

A password change, an admin's reset of the password, a Navidrome sign-in that links an account which had no Navidrome link, or linking or unlinking single sign-on disconnects every app of that user. Every new connection starts with deletion off.

Access tokens last 1 hour. Refresh tokens last 60 days from their issue, and each refresh gives a new one, so an app used at least once in 60 days stays connected. A refresh token used again more than a minute after it was replaced ends the connection.

When nothing is connected the panel says "No app is connected. Add {your address}/mcp as a remote MCP server in your AI assistant to connect one."

## Deleting music

File management (deleting, moving across folders, and re-tagging) is off for every app until its owner turns on **Allow managing library files (delete, move, re-tag)** in Settings > Users > Connected apps. With it on, an app can:

- delete songs you confirmed: `preview_delete_tracks` checks up to 25 songs and says for each whether it may be deleted and, if not, why, with a confirmation token for exactly the songs it allows; the assistant shows you the preview and asks; `delete_tracks` with that token deletes them after your yes. `find_duplicates` finds songs your library holds more than once, to preview;
- move tracks or whole albums between your folders (`move_tracks`) while preserving file bytes, filename, beets libraries, Navidrome song ids, ratings, stars, play counts and static playlists; dry-run previews (`dry_run: true`) and `inspect_tracks` work without the switch;
- re-tag or re-match existing library tracks through beets (`retag_tracks`), in place by default so Navidrome keeps the same song id (or with `relocate: true` using a two-scan dance when an admin account can scan); dry-run previews work without the switch;
- replace a library copy while it downloads a better one: `ensure_in_library`'s `replace_existing` takes the token of a preview with reason `replaced`, and the previewed copies are deleted once the new copy is imported, within 24 hours;
- run your rating sync (`sync_ratings`) with its auto-delete: when you turned auto-delete on, the songs you rated 1 star are deleted;
- keep a discovery track (`promote_discovery_track`) that replaces an earlier copy of the song in your library;
- decide an import the web page started (`resolve_import`) that replaces an earlier copy in your library.

The last three delete without a preview. They count against the same daily limit and appear in the deletion history under the app's name.

Without the switch, `preview_delete_tracks`, `delete_tracks`, `move_tracks` (`dry_run: false`) and `retag_tracks` (`dry_run: false`) answer `permission_not_granted` with "Managing library files (delete, move, re-tag) is turned off for this app" and the user action "Allow managing library files for this app" in "Settings > Users > Connected apps"; `ensure_in_library` refuses `replace_existing` the same way; `sync_ratings` skips auto-delete (the 6-hourly sync does it); `promote_discovery_track` and `resolve_import` replace no earlier copy and end `import_already_in_library`.

Some changes are refused to every app, whether or not its deletion switch is on, because they would let a later sync delete files without a preview:

- turning auto-delete on: "Only you can turn auto-delete on, in Soulbeet's Settings: it deletes the files of songs you rate 1 star at every rating sync";
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

What each tool does, in the order an MCP client sees them. Every tool answers one JSON document, and a failure has a stable code (see [Error codes](#error-codes)). Every time a tool answers or takes is RFC 3339 in UTC to the second, such as `2026-11-15T10:00:00Z`.

### Status, settings and folders

- `get_status`: what Soulbeet can do for this user now: whether each enabled download backend and beets work (`download_backends`, one `{id, name, online, failure}` per backend, the default first; `download_backend` describes the default one, slskd), the Navidrome link and scan status, whether Soulbeet can start a Navidrome scan after the user's imports (`can_trigger_scan`, with `scan_note` saying how, see [Navidrome scans](#navidrome-scans)), the folders with free space and `in_navidrome` (`listed` when a Navidrome library Soulbeet has located on this machine holds the folder, so imports into it show up in Navidrome; `unknown` otherwise), the batches still running, whether this app may delete music, and Soulbeet's address.
- `get_settings`: the user's settings: default folder, auto-delete, ListenBrainz name (never the token), discovery settings, with `discovery.deezer_recommendations` (whether the names of artists the user plays go to Deezer).
- `list_folders`: the user's library folders with ids, paths, free space, whether shared, `in_navidrome`, and which is the default.

### Library

- `get_library_stats`: counts of tracks, albums and artists, the user's ratings, and the 20 most common genres.
- `find_in_library`: looks up to 100 tracks, by recording MBID, then by artist and title; matches are `mbid`, `exact` or `fuzzy` (left for the assistant to judge) and include each song's Soulbeet `folder` when known.
- `search_library`: free text search of the library for songs, albums or artists.
- `get_library_album`: one album with its songs in order.
- `list_starred`: the songs, albums or artists the user starred.
- `list_playlists`: the user's playlists and other users' public ones.
- `get_playlist`: one playlist with its entries in order, including each song's Soulbeet `folder` when known.

### Music metadata

- `search_music`: searches MusicBrainz for recordings, releases or artists, to check that a recording exists and get its ids. With kind `release`, an `artist` and no `query` (or `*`), it lists that artist's albums, EPs and singles, studio albums first, singles last and each part oldest first, and names in `other_artists` the other artists of that name; pass one's `mbid` as `artist_mbid` to list theirs. The listing comes in pages that `cursor` follows, and `complete` is true on its last page when it held every album, EP and single MusicBrainz has for the artist.
- `get_release`: a release with its tracks, by MusicBrainz release id.
- `get_recording_releases`: a recording and the releases it appears on, to tell a studio album from a compilation or a live record.

### Downloads

- `find_sources`: searches Soulseek through slskd for a track or an album and ranks what it finds; searches run one at a time for the whole server. Another download backend an admin turned on, such as the Internet Archive or YouTube Music, is not searched here nor by the jobs this tool's results start: the web app's auto-download searches it. A search no peer answers at all is sent again with wildcards (see [Known limits](#known-limits)), and `facts.query_texts_retried` lists the texts sent after `query_text_sent`.
- `get_sources`: reads a search `find_sources` started, a page at a time.
- `get_jobs`: shows jobs by id or by batch: state, current download, earlier attempts, import, and the calls that can be made next.
- `get_batch`: follows a batch, and can wait up to 30 seconds for a change; for a batch with a playlist, how far the playlist is.
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

- `create_playlist`: creates a static playlist with the given `song_ids` in order, or a Navidrome smart playlist with `.nsp` JSON `rules` (`all`/`any`, operators, optional `sort`/`order`/`limit`/`offset`; mutually exclusive with non-empty `song_ids`).
- `update_playlist`: renames a playlist, changes its comment or public flag, or updates a smart playlist's `rules`.
- `delete_playlist`: deletes one of the user's playlists; the songs stay.
- `set_playlist_tracks`: replaces every entry of a playlist, which also reorders it.
- `add_playlist_tracks`: adds songs at the end or before a position.
- `remove_playlist_tracks`: removes entries, each named by its position and song.

### Ratings, stars and settings changes

- `set_rating`: sets the user's own rating of a song or album; a 1-star song rating and any rating of a pending discovery track other than clearing it are refused.
- `set_star`: stars or unstars a song or album.
- `update_settings`: changes the default folder, turns auto-delete off, and changes discovery's on/off, profiles, track counts, lifetimes and playlist names.

### Discovery

- `get_discovery`: the user's discovery: folder, profiles, the current run and the pending tracks. A run that ended in error has its failure in `run.failure` for 5 minutes. `last_run` is the newest recommendation run, the automatic 6-hourly one included: `state` (`recommended` or `no_listening_data`), the `sources` it used and the `source_errors` of those that failed.
- `run_discovery`: starts a discovery run. Recommendations come from the user's Navidrome plays and stars, from ListenBrainz when a username is saved, from data about music from ListenBrainz and MusicBrainz, and from Deezer when the user turned it on. The call returns as soon as the run starts; its own failures are only `invalid_input` (discovery off), `folder_required` and `already_running`. A run that cannot build recommendations ends with `run.status` `error`, and `get_discovery` shows the reason in `run.failure`: `no_listening_data` with nothing played or starred in Navidrome (and no ListenBrainz history), `metadata_unavailable` naming a listening history such as ListenBrainz that could not be read while nothing else has plays or stars (retry later), or `navidrome_not_linked` / `navidrome_unreachable` when the Navidrome library could not be read.
- `promote_discovery_track`: moves a pending discovery track into the library (optionally into another folder owned by the user via `target_folder_id`); it replaces an earlier copy only with the file-management switch on.
- `remove_discovery_track`: deletes a pending discovery track from the discovery folder.
- `sync_ratings`: runs the user's rating sync now, as Soulbeet does every 6 hours.

### Deletion

- `find_duplicates`: finds songs the library holds more than once, grouped by artist and title, with each song's Soulbeet `folder` when known.
- `preview_delete_tracks`: checks up to 25 songs for deletion and gives a confirmation token for the ones that may go; deletes nothing.
- `delete_tracks`: deletes the songs a confirmation token allows, from disk and from beets; there is no undo.
- `get_deletion_history`: the deletions of the user's files, newest first, with why, how and which app.

### Track management

- `inspect_tracks`: inspects up to 25 library tracks: exact local path, Soulbeet folder, Navidrome library id, beets database tags, album peers and album rating/star, caller's playlists, other-listener visibility, sidecars (`.lrc`, cover art) and discovery state.
- `move_tracks`: moves up to 25 songs (`song_ids`) or one whole album (`album_id`, up to 100 tracks) to another of the user's folders (`target_folder_id`) while preserving file bytes, filename, beets libraries, Navidrome song id, ratings, stars, play counts and static playlists. Supports `dry_run: true` and `replace_existing: true` (through the delete guard).
- `get_move_history`: lists the user's track moves between folders, newest first, with source and target folders/paths, status (`moved`, `moved_pending_scan`, `moved_id_changed`) and warnings.
- `retag_tracks`: edits tags directly (`tags`) or re-matches against MusicBrainz via beets (`rematch`) for up to 25 songs or one album; keeps files in place by default so Navidrome keeps the same song id, or runs a two-scan dance with `relocate: true` when an admin can scan. Supports `dry_run: true`.

### Library browsing, insights and maintenance

- `scan_library`: triggers a quick or full (`full_scan: true`) Navidrome library scan (automatically using another linked admin account if the caller is not an admin), optionally waiting up to `wait_seconds` (max 30) for completion.
- `browse_library`: paginated, filterable listing of songs or albums (`kind`: `songs` or `albums`) with filters for `folder_id`, `genre`, `min_rating`/`max_rating`/`unrated`, `starred`, `year_from`/`year_to`, `format`, `max_bitrate_kbps`, `missing`, and sorting (`recently_added`, `recently_played`, `most_played`, `highest_rated`, `title`, `artist`, `year`, `random`).
- `get_library_artist`: shows one artist in the user's Navidrome library with a paginated list of their albums.
- `get_listening_history`: read-only summary of recent plays, top tracks, top artists, top genres, and (when configured and `include_listenbrainz` is true) ListenBrainz recent listens and top artists.
- `get_recommendations`: read-only preview of Soulbeet's recommendation engine (`Conservative`, `Balanced`, or `Adventurous`) returning ranked candidate tracks, signals and seed artists without downloading or modifying discovery state.
- `audit_library`: read-only health audit of the user's library folders against Navidrome, reporting untracked audio files on disk, missing Navidrome files, albums split across folders/libraries, missing recording MBIDs, missing cover art, and missing lyrics.
- `fetch_artwork_and_lyrics`: fetches missing album cover art (Cover Art Archive / MusicBrainz) and/or synced/plain `.lrc` lyrics (LRCLIB) for up to 25 songs or one album, writing files atomically inside the caller's owned folders and triggering a Navidrome scan. Supports `dry_run: true`; `overwrite: true` requires the file-management switch.

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
| Waits (`wait_seconds`) | up to 30 seconds per call |
| Navidrome lookups after a wait | 20 seconds per call in all; the rest fail `navidrome_unreachable` in `library_songs_failure` |
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
| Refresh token | 60 days from its issue; a connection keeps its newest 100 used ones to catch a reuse, and an older one is refused as unknown |
| Sign-in request | 10 minutes |
| Pending sign-ins | 20 per user |
| Connected apps | 20 per user; a new one ends the least recently used |
| Registered apps | 100 on the server |
| App registrations | 30 an hour on the server |
| Token requests | 30 a minute per user |
| Refreshes | 10 a minute per connection |
| Client metadata fetches | 10 a minute per user |

claude.ai's own limits: it was seen to drop a tool call after 90 seconds (November 2026), and a result about 150,000 characters.

## Troubleshooting

### claude.ai says "Couldn't reach the MCP server"

This is claude.ai's message. Its servers could not reach your address. Other hosted clients fail the same way, with their own wording. Check that the name has a public `A` record (see [Requirements](#requirements)), that your firewall and any WAF let the client's addresses through (`160.79.104.0/21` for claude.ai), and that the address does not redirect to another host.

### claude.ai says "Authorization with the MCP server failed"

This is also claude.ai's message.

The sign-in started but did not finish. Two causes are common: Soulbeet's token endpoint took more than 10 seconds to answer, or the address in the client is not the address Soulbeet derives for its requests. Open [Remote access](#check-the-address) and compare.

### A connection answer with an `error` code

The OAuth and `/mcp` endpoints answer a refusal as `{"error": code, "message": text}`.

- `mcp_disabled` (404): "MCP access is turned off: MCP=off is set", or "MCP access is turned off: MCP is set to '{value}', which is neither on nor off". Set `MCP=on` or remove it.
- `mcp_unavailable` (503): "MCP access stays off until the security checks in Settings pass". The answer does not say which check fails: an admin sees that in Settings > Config under Security Checks, with the texts quoted under [Requirements](#requirements). Fix what it lists.
- `address_unreadable` (400): "Cannot tell this request's address: {problem}", where the problem is one of "The request names no host (Forwarded host=, X-Forwarded-Host or Host)", "'{value}' is not a valid host", "'{value}' is not http or https", "The address must not contain a user name or password", "The address must not have a path: Soulbeet cannot run under a sub-path", or "PUBLIC_URL is not usable: {problem}". Fix the proxy header or `PUBLIC_URL` it names.
- `https_required` (403): "Soulbeet derived the address {address} for this request. MCP clients connect only over https: have the proxy send X-Forwarded-Proto: https (or Forwarded: proto=https), or set PUBLIC_URL." The answer also gives `derived_base`, `host_from` and `scheme_from`.
- `origin_not_allowed` (403): "Requests from {origin} are not accepted". A request to `/mcp` came with an `Origin` that is plain http and neither Soulbeet's own address nor loopback, or that is not an origin at all (`null`). MCP clients are not affected; this guards against DNS rebinding.
- `settings_unreadable` (503): "Soulbeet's database is unavailable". Check the server's disk and log.

When `PUBLIC_URL` is set but unusable, nobody can sign in, and Settings > Config says "PUBLIC_URL is not usable: {problem}. It must look like https://host or https://host:port. Nobody can sign in until it is fixed or removed."

### `/mcp` answers 401

The `WWW-Authenticate` header names `invalid_token` with one of these descriptions:

- "No access token was sent": the first request of every connection; the client then signs in. If it keeps getting it, the proxy drops the `Authorization` header.
- "The access token is unknown, expired or revoked": the app was disconnected, the user's password changed, or the token expired and could not be refreshed. Connect again.
- "The access token was issued for another address of this server": the app connected at one address (for example a LAN address) and now calls another. Connect again at the public address, or set `PUBLIC_URL`.

### An error page while connecting

Soulbeet's sign-in and consent pages show these texts:

- "Soulbeet does not know this app. Start the connection again from the app."
- "This app asked to return to an address it did not register."
- "Your Soulbeet session ended. Start the connection again from the app."
- "This sign-in request expired or was already used. Start the connection again from the app." A sign-in request lasts 10 minutes. Clicking Allow (or Deny) twice is fine: for 30 seconds the same answer from the same browser goes back to the app the same way.
- "Soulbeet did not understand the answer. Start the connection again from the app."
- "Soulbeet's database is unavailable. Start the connection again from the app."
- "This app sent {parameter} more than once. Start the connection again from the app."
- "This app sent a request Soulbeet cannot accept ({error}: {description}). Start the connection again from the app." With `invalid_target` the description is "resource must be {address}/mcp": the address in the client differs from the address Soulbeet derives. Make them the same (see [Check the address](#check-the-address)).
- "Soulbeet could not use this app's client metadata at {url}: {problem}"
- "Too many app sign-ins in a minute. Try again in {n} seconds."

### OAuth errors

The registration and token endpoints answer `{"error", "error_description"}`:

- `rate_limited` at registration: "Too many app registrations on this server. Try again in {n} seconds."
- `too_many_clients`: "Soulbeet holds 100 registered apps, all connected. Disconnect one in Settings > Users > Connected apps."
- `rate_limited` at the token endpoint: "Too many token requests for this Soulbeet account. Try again in {n} seconds." or "Too many refreshes for this connection. Try again in {n} seconds."
- `invalid_target`: "resource must be {address}/mcp", as above, or "The refresh token was issued for another address of this server".
- `invalid_grant`: "The refresh token is unknown, expired or revoked", or "The refresh token was already used; the connection is ended". Connect again.

### Tool errors

A tool that fails answers its document with `ok: false` and an `error` with these fields:

- `code`: one of the [error codes](#error-codes);
- `stage`: where it failed: `input`, `metadata`, `library_check`, `source_search`, `enqueue`, `transfer`, `file_lookup`, `import`, `index`, `playlist`, `delete`, `move` or `retag`;
- `message`: what happened;
- `cause`: the other service's own answer, when there is one (see [Failure causes](#failure-causes));
- `retry`: whether and how to try again: `same_source_later` (with `retry_after_seconds` when known), `other_source`, `other_query`, `needs_decision`, `needs_user`, `not_retryable`, or `per_item` for a `partial` answer;
- `user_action`: when only a person can fix it, what to do and where (see [User actions](#user-actions));
- `facts`: what Soulbeet knew when it failed, such as the ids involved;
- `tried`: earlier attempts and how each failed, when there were any;
- `next`: the calls that can still be tried.

The ones you are most likely to see:

- `rate_limited`: "Too many tool calls from this app: at most 120 a minute. Try again in {n} seconds." The assistant waits and tries again.
- `permission_not_granted`: file management is off for the app ("Managing library files (delete, move, re-tag) is turned off for this app"), or the change is one no app may make (see [Deleting music](#deleting-music)).
- `delete_daily_limit`: "At most 100 tracks a day can be deleted through connected apps, and {used} were deleted in the last 24 hours. {n} more can be deleted from {time}."
- `expired_handle` for a confirmation: "This confirmation token was already used" or "This confirmation token expired at {time}; preview the songs again". The assistant makes a new preview.

## Error codes

| Code | Stage | What it means | What to do |
|---|---|---|---|
| `invalid_input` | `input` | An argument is missing, has the wrong type or range, or is one the tool does not take; `facts.expected` names what it takes. Also a request the state does not allow, such as running discovery while it is off. | The assistant corrects the call. |
| `unknown_handle` | `input` | An id that names nothing this user may use: a job, batch, search, candidate, folder, song, album, discovery track, confirmation token or connected app. Another user's id reads the same. | Get the id again from the tool that lists it. |
| `expired_handle` | `input`, `import`, `delete` | The id was valid but is no longer usable: a search older than 24 hours, a confirmation token used or older than 10 minutes, a replacement consent past its 24 hours, a discovery track no longer pending. | Start again from the step that gave it: a new search or a new preview. |
| `permission_not_granted` | `input`, `delete`, `move`, `retag` | The app may not do this: file management is off for it (user action `enable_deletion`), or no app may make the change: auto-delete on (`enable_auto_delete`), the discovery folder or promote threshold (`change_discovery_settings`), a 1-star song rating or a rating of a pending discovery track (`rate_in_navidrome`). | Do what the user action says: tick Allow managing library files (delete, move, re-tag) in Settings > Users > Connected apps, change the setting in Settings > Library, or rate in Navidrome. |
| `folder_required` | `input` | There is no folder to download into: no `folder_id` and no default folder, no library folder at all, or no discovery folder. User action `add_folder` when one is missing. | Add a folder or choose a default in Settings > Library, or a discovery folder in Settings > Library > Discovery. |
| `no_listening_data` | `input` | Discovery found nothing played or starred in Navidrome, and no ListenBrainz history, every history having answered. User action `listen_in_navidrome`. | Play or star music in Navidrome, then run discovery again. |
| `rate_limited` | `input`, `source_search`, `enqueue`, `transfer` | Too many tool calls from this app, 3 source searches already waiting for this user, or 200 unfinished app jobs for this user; or YouTube rate-limited this server, which holds every YouTube Music download for an hour (`retry_after_seconds` says how long is left); or GD Studio refused a download's call for too many requests. | Wait; `retry_after_seconds` says how long where it is known. |
| `metadata_unavailable` | `metadata`, `input` | MusicBrainz did not answer or answered with an error. At stage `input`, a discovery run had no listening data to start from while a listening history such as ListenBrainz failed; the message names it. | Try again later. |
| `metadata_not_found` | `metadata` | MusicBrainz has no such recording, release or artist, or a download names an album from a Last.fm search, which Soulbeet no longer uses. | Search again with other words or another id. |
| `metadata_rate_limited` | `metadata` | MusicBrainz refused for too many requests. | Try again later. |
| `navidrome_unreachable` | `library_check`, `index`, `playlist`, `delete` | Navidrome did not answer, answered an HTTP error (often a proxy or a wrong `NAVIDROME_URL`), or answered something that is not its API. From `find_duplicates`: Navidrome's pages held fewer songs than it counts. | Check that Navidrome runs and `NAVIDROME_URL` is right; try again. |
| `navidrome_auth_failed` | `library_check`, `index`, `playlist`, `delete` | Navidrome refused the stored Navidrome password. User action `relink_navidrome`. | Enter your current Navidrome password under Connect Navidrome, in Soulbeet's Settings > Users. |
| `navidrome_not_linked` | `library_check`, `index`, `playlist`, `delete` | The account is not linked to Navidrome or its stored password cannot be read (user action `relink_navidrome`), or `NAVIDROME_URL` is not set (user action `configure_navidrome`). | Enter your Navidrome password under Connect Navidrome, in Soulbeet's Settings > Users, or set `NAVIDROME_URL`. |
| `navidrome_circuit_open` | `library_check`, `index`, `playlist`, `delete` | Navidrome failed 5 times in a row, so Soulbeet sends it nothing for 60 seconds. | Wait a minute; check Navidrome. |
| `navidrome_refused` | `library_check`, `index`, `playlist`, `delete` | Navidrome answered with an error other than a refused login, for example an account that may not do what was asked. | Read `cause`; trying again gives the same answer. |
| `backend_not_configured` | `source_search`, `enqueue`, `transfer` | The slskd URL or API key is missing, does not parse, or slskd refused the key; or the download backend is turned off (user action `configure_backend`); or the Internet Archive or NetEase (GD Studio) backend cannot keep its partial files in `DOWNLOAD_PATH` (user action `fix_download_path`), or another Soulbeet server on the same database and downloads folder holds them; or the image lacks yt-dlp, ytmusicapi, deno or ffmpeg for YouTube Music ("Not in this image: use the :full image"); or the AirDC++ address, user or password of the Direct Connect backend is missing or refused, its web user lacks a permission, AirDC++ runs in another language than English, or it does not save into `DOWNLOAD_PATH` (user action `fix_download_path`). | An admin sets the slskd URL and API key in Settings > Config, or turns the backend on in Settings > Config > Connections. |
| `backend_unreachable` | `source_search`, `enqueue`, `transfer` | slskd did not answer, is not connected to the Soulseek network, or is not logged in (then user action `configure_backend`: check slskd's Soulseek user name and password); or archive.org, YouTube, GD Studio or AirDC++ did not answer (GD Studio is left alone for a minute after 5 failed calls in a row), YouTube asks this server to sign in, or AirDC++ is connected to no hub. At `source_search` also: slskd ended the search with an error before any peer answered. | Check slskd; try again later. |
| `settings_unreadable` | any | Soulbeet could not read its own database. | Check the server's disk and log; try again later. |
| `backend_response_unreadable` | `source_search`, `enqueue`, `transfer`, `metadata` | slskd, MusicBrainz or GD Studio answered something Soulbeet cannot read. For slskd, user action `update_soulful_or_backend`; for GD Studio too when its API refuses unsigned requests (401). GD Studio naming a file outside NetEase's servers fails the transfer with this code. For YouTube Music, yt-dlp failed on the track or its helper printed something Soulbeet cannot read: an image with a newer yt-dlp usually fixes it. | Update Soulbeet or slskd so that their versions match. |
| `search_rate_limited` | `source_search` | slskd refused the search for too many requests, archive.org kept answering 429, YouTube rate-limited this server, GD Studio answered 429, or AirDC++ holds more than 20 minutes of searches. | Try again later. |
| `search_no_results` | `source_search` | Nothing on the Soulseek network matched, or the Soulseek server dropped the search. When no peer answered at all, even with the wildcards Soulbeet tried, the message says the server may be blocking it. | Search with other words: write a word with `*` in place of its first letter (`*GMT Kids`), keeping one word as it is in a text of three words or more, or leave a word out. |
| `search_all_filtered` | `source_search` | Results came back, but each one scored too low or was cut by the filters; the facts count each. | Loosen the filters (for example `lossless_only`) or change the query. |
| `search_timeout` | `source_search` | The search did not end in time. | Try again later. |
| `search_query_rejected` | `source_search` | slskd refused the search text as unusable, for example one of exclusion terms only, or archive.org refused the query. | Search with other words. |
| `search_already_running` | `source_search` | slskd already runs a search under the id asked for. | Try again later. |
| `search_lost` | `source_search` | slskd no longer has the search Soulbeet was reading. | Run the search again. |
| `peer_offline` | `enqueue`, `transfer` | The source is offline. For Direct Connect: the peer is not on its hub when the file is queued, or was offline for the 5 minutes its file got no byte. | Another source, or the same one later. |
| `already_running` | `input`, `enqueue` | The same work is already going: a job for this recording, a call with this `request_key`, a rating sync or discovery run of this user, or a download of this user taking the same file into this folder. | Follow the one that runs. |
| `already_in_flight_other_user` | `enqueue` | Another user's download takes the same file. | Another source. |
| `already_in_flight_other_folder` | `enqueue` | This user already downloads the same file into another folder. | Wait for it, or another source. |
| `enqueue_failed` | `enqueue` | slskd did not take the download: it could not reach the source, its blacklist holds the source, or it refused the request. For YouTube Music, the user already has 200 downloads waiting there, or the track is already being fetched into the same folder. | Another source, or as `retry` says. |
| `slskd_too_old` | `enqueue` | slskd is older than 0.26. User action `upgrade_backend`. | Upgrade slskd to 0.26 or newer. |
| `transfer_connection_failed` | `transfer` | The connection to the source failed or dropped, or slskd restarted during the transfer. For archive.org, 5 connections in a row brought no byte; for YouTube, 3 tries in a row failed. For Direct Connect, AirDC++ could not connect to the peer for 5 minutes. | Another source; after a slskd restart, the same one later. |
| `transfer_size_mismatch` | `transfer` | The source's file has another size than it announced (for archive.org, twice), or the file YouTube sent is more than 2 seconds off the track's length. | Another source. |
| `transfer_checksum_mismatch` | `transfer` | The downloaded bytes do not match the checksum the source gave for the file (for GD Studio, the MD5 NetEase names the file by; for Direct Connect, AirDC++ found bytes that do not match the file's TTH). | Another source. |
| `source_file_changed` | `transfer` | The source now serves another file than the one the search found: GD Studio names another NetEase file for the track. Nothing was downloaded. | Search again to see what is served now. |
| `transfer_rejected_not_shared` | `transfer` | The source says the file is not shared; archive.org refused the file (restricted) or no longer has it; YouTube Music no longer has the track, or not in this country, or asks for an age check. | Another source. |
| `transfer_rejected_peer_busy` | `transfer` | The source's queue is full, or it hit a daily or weekly quota; or archive.org answered 429 or 503 six times, or asked for a wait over 10 minutes (then `retry_after_seconds`). | The same source later for a full queue, another source for a quota. |
| `transfer_rejected_failure_limit` | `transfer` | The source refuses after too many failed transfers today or this week. | Another source. |
| `transfer_rejected_other` | `transfer` | The source refused for another reason, given in `cause`; or the file NetEase sent is not the FLAC or MP3 its name says, or could not be tagged. For YouTube: it asks this server to sign in (every YouTube Music download waits 6 hours, unless a check passes first), refused the file (HTTP 403), or has no Opus format of 96 kbps or more for it. | Another source. |
| `transfer_timed_out` | `transfer` | The transfer timed out in slskd, or ran past Soulbeet's time for one track, or the YouTube Music helper sent nothing for 2 minutes or ran for 10; or NetEase's server stopped sending the file for a minute, three times. | Another source. |
| `transfer_aborted` | `transfer` | slskd aborted the transfer, the Internet Archive or YouTube Music backend or AirDC++ could not write the file into the downloads folder, or the NetEase (GD Studio) backend could not write, tag or land it there (user action `fix_download_path`). | Another source. |
| `transfer_cancelled` | `transfer`, `import` | The transfer was cancelled, in Soulbeet or in slskd. | Nothing, or retry the job. |
| `transfer_never_appeared` | `transfer` | slskd took the request but never listed the transfer. | Another source. |
| `transfer_disappeared` | `transfer` | The transfer vanished from slskd's list. | The same source later. |
| `transfer_stuck_in_remote_queue` | `transfer` | The transfer waited 15 minutes in the source's queue. | Another source. |
| `transfer_list_unreadable` | `transfer` | slskd's list of downloads could not be read, or the Internet Archive backend could not read or write its transfer records in Soulbeet's database. | Check slskd; update Soulbeet or slskd if their versions do not match. |
| `file_not_found_after_download` | `file_lookup` | slskd finished, but Soulbeet cannot find the file where it looks. User action `fix_download_path` when the file is not on disk. For archive.org, a file that landed before a restart is no longer in its batch folder; it is not downloaded again. | Mount slskd's download folder in Soulbeet at `DOWNLOAD_PATH`. |
| `download_disk_full` | `transfer` | The disk the download backend writes to is full. User action `free_disk_space`. | Free disk space, then retry. |
| `audio_corrupt` | `verify` | The downloaded audio file is empty, unreadable, or failed container or packet decoding. | Another source. |
| `audio_silent` | `verify` | The downloaded audio file is digitally silent across its interior. | Another source. |
| `audio_truncated` | `verify` | The downloaded audio file is much shorter than the expected track duration (such as a preview clip). | Another source. |
| `audio_fake_lossless` | `verify` | An archival lossless candidate failed spectral inspection because it shows a sharp lossy brickwall cutoff. | Another source. |
| `replacement_duration_mismatch` | `delete` | The newly imported copy's duration differs too far from the existing library file, so the existing copy was kept. | Inspect both copies before deleting the earlier one manually. |
| `import_no_match` | `import` | beets found no close enough match. | The assistant reads `get_import_choices` and decides with `resolve_import`, or you decide in the web page. |
| `import_already_in_library` | `import`, `index` | The library already holds this recording; the facts give its song ids. | Nothing. To replace it, preview with reason `replaced` and pass `replace_existing`. |
| `import_nothing_to_import` | `import` | beets found nothing to import in the downloaded files. | Another source. |
| `import_lookup_failed` | `import` | beets could not look the files up. | Try again later. |
| `import_beets_failed` | `import` | beets failed; `cause` holds the end of its output. | Try again later; read beets' output. |
| `import_target_unwritable` | `import` | beets cannot write into the library folder. | Fix the folder's permissions. |
| `import_disk_full` | `import` | The disk beets writes to is full. User action `free_disk_space`. | Free disk space, then retry. |
| `import_library_locked` | `import` | The beets library database stayed locked: another program had it open, or it sits on a network share, where SQLite cannot lock files. Soulbeet tries the import again by itself a few times; `retry_after_seconds` says when the next try is. | Wait for the next try. When the tries run out, close the other program, or keep Soulbeet's data folder on a local disk ([beets.md](beets.md#library-databases)), then retry. |
| `import_idle_timeout` | `import` | beets printed nothing for too long and was stopped. | Try again later. |
| `import_total_timeout` | `import` | beets ran past its time limit and was stopped. | Try again later. |
| `importer_broken` | `import` | beets is not set up so that Soulbeet can run it. User action `fix_importer`. | Fix the beets setup; Settings > Config shows the problem. |
| `import_as_is_missing_tags` | `import` | An as-is import was refused: some files lack artist and title tags (and album, in album mode). | Choose a match instead. |
| `import_choice_invalid` | `input`, `import` | The chosen candidate is not one beets found, or a MusicBrainz link names the wrong kind of entry. | Choose again. |
| `import_files_gone` | `import` | The downloaded files kept for the decision were deleted. | Dismiss the job, or download again. |
| `index_not_found_after_scan` | `index` | Navidrome scanned after the import and does not list the file. | Check that a Navidrome library holds the folder (`in_navidrome`), then retry with `index_only`. |
| `index_no_scan_possible` | `index` | Navidrome has not scanned for 10 minutes since the import, and no Navidrome account linked to Soulbeet may start a scan. User action `enable_indexing`. | Sign in to Soulbeet once as a Navidrome admin, turn on Navidrome's file watcher, or set a scan schedule. |
| `scan_failed` | `index` | Navidrome's scan failed. | Try again later. |
| `playlist_not_found` | `playlist` | No such playlist, or another user's private one. | List the playlists again. |
| `playlist_not_owner` | `playlist` | Only the owner of the playlist can change it. | Make a copy of your own. |
| `playlist_name_taken` | `playlist` | You already have a playlist with this name. `facts.existing_playlist_ids` names them, and `next` offers each before a second playlist of the name. | Use the existing playlist, another name, or `allow_duplicate_name`. |
| `playlist_tracks_not_written` | `playlist` | Navidrome does not list the playlist's songs as they were written. | Read the playlist and try again. |
| `playlist_not_written` | `playlist` | Navidrome answered a change of name, comment, public flag or a delete, but does not show it. | Read the playlist and try again. |
| `playlist_changed` | `playlist`, `input` | A position given no longer holds the song given. | Read the playlist again. |
| `playlist_not_editable` | `playlist` | A smart or file-synced playlist, whose tracks Navidrome sets itself. | Use a static copy. |
| `delete_path_unresolved` | `delete`, `library_check` | Soulbeet cannot find the song's file where Navidrome's library puts it; the message says why. "No folder at this song's library root" means Soulbeet looks for the library in the wrong place. For `set_rating` while you have pending discovery tracks, user action `configure_navidrome` for that reason and `rate_in_navidrome` for any other. | For a missing library root: make Navidrome's music folder readable by Soulbeet at Navidrome's path, or set `NAVIDROME_MUSIC_PATH`. Otherwise: the file is gone, moved or changed; rate the song in Navidrome. |
| `delete_outside_library` | `delete` | The file is not inside one of your library folders, or may be inside another user's. | Nothing; only files in your folders can be deleted. |
| `delete_not_a_track` | `delete`, `library_check` | The file is not an audio file. | Nothing. |
| `delete_blocked_by_other_user` | `delete` | Another listener rated, starred or kept this track in a playlist. | Nothing; Soulbeet does not say who. |
| `delete_veto_unknown` | `delete` | Soulbeet cannot rule out a mark it cannot read: Navidrome is older than 0.60 or its version cannot be read, a listener's ratings could not be read, a Navidrome user has no Soulbeet account, or a linked account cannot see the track. User action `relink_navidrome` or `configure_navidrome` where one helps. | Do what the user action says: have every Navidrome user sign in to Soulbeet once, sign in once as a Navidrome admin, or give every linked account access to the track's library. |
| `delete_in_use` | `delete` | A download or import that is not finished wants this track. | Wait until it ends. |
| `delete_changed_since_preview` | `delete` | The file changed after the preview, or was not in it. | Preview again. |
| `delete_failed` | `delete`, `import` | The system refused to remove a file every check allowed; the message gives its reason. | Fix the file's permissions. |
| `delete_daily_limit` | `delete`, `move` | 100 tracks were deleted through apps in the last 24 hours, or more than 100 were asked at once. | Wait until the time the message names. |
| `move_same_folder` | `move` | The track is already inside the requested target folder. | Nothing to move; choose another folder. |
| `move_target_exists` | `move` | A file already exists at the exact destination path in the target folder. | Pass `replace_existing: true` after confirming with the user, or inspect the existing file first. |
| `move_duplicate_in_target` | `move` | The target folder already contains a copy of this song (matching recording MBID or normalized artist + title); `facts.existing_in_target` lists the existing copies. | Compare both copies with the user; pass `replace_existing: true` if they want to replace the target copy. |
| `move_failed` | `move` | Moving the track on disk failed (for example permissions or disk full); the message gives the OS error. | Fix the filesystem error and try again. |
| `retag_failed` | `retag` | Updating tags, re-matching or relocating the track through beets failed. | Check beets output in `cause` or retry. |
| `retag_relocate_needs_scan` | `retag` | `retag_tracks` was called with `relocate: true`, which requires a linked Navidrome admin account to run the two-scan dance so Navidrome keeps the same song id. | Re-tag in place (`relocate: false`), or link a Navidrome admin account to Soulbeet. |
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
| `configure_backend` | "Set the slskd URL and API key", "Check the Soulseek username and password slskd signs in with", or "Turn on {backend}" | "Settings > Config", "slskd", or "Settings > Config > Connections" |
| `upgrade_backend` | "Upgrade slskd to 0.26 or newer" | "slskd" |
| `update_soulful_or_backend` | "Update Soulbeet or slskd so that Soulbeet can read slskd's answers again" | "Soulbeet and slskd" |
| `fix_importer` | "Fix the beets setup: {problem}" | "Settings > Config" |
| `fix_download_path` | "Make the folder the download backend saves into readable by Soulbeet at {DOWNLOAD_PATH}" | "DOWNLOAD_PATH and the volume mounted there" |
| `free_disk_space` | "Free disk space: {message}" | "The disk that holds the path in the message" |
| `relink_navidrome` | "Enter your current Navidrome password under Connect Navidrome", "Sign in to Soulbeet once as a Navidrome admin, so Soulbeet can list Navidrome's users", or "Have every Navidrome user sign in to Soulbeet once, so Soulbeet can read their ratings" | "Soulbeet's Settings > Users" for the first, "Soulbeet sign-in" for the others |
| `configure_navidrome` | "Set NAVIDROME_URL to the address of your Navidrome server", "Make Navidrome's music folder readable by Soulbeet at Navidrome's library path, or set NAVIDROME_MUSIC_PATH to where Soulbeet sees Navidrome's default library", or "Give every Navidrome account linked to Soulbeet access to this track's library, so Soulbeet can read their ratings of it" | "Soulbeet's environment", or "Navidrome's user settings" |
| `enable_deletion` | "Allow managing library files for this app" | "Settings > Users > Connected apps" |
| `enable_indexing` | "Sign in to Soulbeet once as a Navidrome admin, enable Navidrome's file watcher, or set a scan schedule" | "Soulbeet sign-in or Navidrome" |
| `add_folder` | "Add a library folder", or "Choose a discovery folder" | "Settings > Library", or "Settings > Library > Discovery" |
| `rate_in_navidrome` | "Rate the song 1 star yourself in Navidrome", or "Rate the song yourself in Navidrome" | "Navidrome" |
| `enable_auto_delete` | "Turn on auto-delete yourself" | "Soulbeet Settings > Library" |
| `change_discovery_settings` | "Change the discovery promote threshold yourself", or "Change the discovery folder yourself" | "Soulbeet Settings > Library" (the threshold), or "Soulbeet Settings > Library > Discovery" (the folder) |
| `listen_in_navidrome` | "Play or star music in Navidrome" | "Navidrome" |

## Failure causes

A failure's `cause.service` names whose answer `cause.raw` holds: `slskd`, `beets`, `navidrome`, `musicbrainz`, `filesystem`, `database` (Soulbeet's own settings database), `internet_archive` (archive.org, for the Internet Archive download backend), `youtube` (YouTube, for the YouTube Music download backend), `gd_studio` (GD Studio's API and NetEase's file servers, for the GD Studio download backend), `airdcpp` (the AirDC++ Web Client of the Direct Connect download backend), or `soulful` (Soulbeet's own words about what it found wrong, or lines it could not read from a service). `cause.status` is the service's status code when it gave one.

## Known limits

Deletion and the checks it passes:

1. Deletion needs Navidrome 0.60 or newer. Below 0.60, or when Navidrome reports no version Soulbeet can read, every deletion and replacement is refused and the file is kept.
2. The song reads need Navidrome 0.55 or newer. An older answer does not parse, the read fails and the file is kept.
3. Stars of Soulbeet users who never linked Navidrome, and of Navidrome users with no Soulbeet account, cannot be read, so they do not keep a track. Their ratings count only through Navidrome's average (check 4). A private playlist of a Navidrome user with no Soulbeet account is seen only when some linked account is a Navidrome admin. A smart playlist never keeps a song, even when its rules name that one track.
4. When some rating Soulbeet reads for a track is above 1 (yours included), the file is kept unless a linked account is a Navidrome admin and every Navidrome user is a linked account that sees the track. So replacing a well rated track keeps both copies on a server where some Navidrome user never signed in to Soulbeet, or where no admin did. Navidrome rounds the average to 2 decimals: with 200 or more ratings of 1, one hidden rating of 2 rounds away.
5. A linked listener whose Navidrome account cannot see a track (per-user libraries) keeps it whenever some rating read is above 1, since a rating they gave before losing access stays in the average and cannot be read. This holds even when they never rated it.
6. A rating or star set between the last check and the removal is not seen: one round trip for a single deletion, longer across the items of one replaced album.
7. A file renamed onto the checked name in the same directory between the last check and the removal is removed instead.
8. Soulbeet finds Navidrome's songs on disk through `NAVIDROME_MUSIC_PATH` for the default library, else through library roots it learns from imported files whose modification time matches what Navidrome recorded. Mounts where Navidrome and Soulbeet read different modification times never learn a root, so deletions there are kept unless the paths are equal or `NAVIDROME_MUSIC_PATH` covers the default library. A root once learned is kept while it is a directory, and a missing file under it is refused ("No file at this song's path"). With no root at all, a copy with the same size, path ending and modification time can teach a wrong root until an import shows the real one.
9. A second song of the file's size in a Navidrome library Soulbeet cannot see (a partial mount) keeps the file.
10. Discovery imports keep beets' own duplicate handling, so beets may remove an earlier discovery item without the deepest folder check (another user's folder registered inside a shared `Discovery/<profile>` folder).
11. A replacement deletes the earlier tracks, not the earlier album's cover file; with fetchart the old cover stays beside the new one.
12. A removal that stopped before it reported, whose path then holds nothing, is recorded as a deletion and its beets item forgotten, also when something else removed the file meanwhile. When the path holds another file, the deletion is recorded and beets' item at that path is left as it is; the server log says so.
13. Any answer of Navidrome's user list other than a list or a refusal for a non-admin counts as unreadable: the file is kept and the refusal says to try later.
14. A stored library root that is no longer a directory, while Navidrome's own path exists on this machine, keeps deletions under that library refused ("No folder at this song's library root") until an import shows a root again.

Downloads and imports:

15. A job whose failure the database refused 7 times waits with no runner until Soulbeet starts again, which resumes or ends it. Until then `next` offers no wait on it.
16. `list_batches` reads every batch of the user to filter by label; fine at a household's size.
17. A job document is kept within 45,000 characters for the cases Soulbeet knows, not proven for every input.
18. A kept import stored before its files were recorded has none: `dismiss_job` and `resolve_import` with `discard` end it and delete nothing.
19. A kept import stays on disk until someone decides it, as in the web page. A file the system refuses to delete stays too and is listed in `files_left`.
20. beets reports every file of a run; a kept import keeps the first 200 and counts the rest in `files_omitted`.
21. The Soulseek server keeps a list of excluded phrases and silently drops every search that holds all the words of one of them, in any order. slskd receives the list but exposes it through no API, so such a search looks like one with no response at all. When no peer answers, Soulbeet sends the search again, at most twice, with some words written as wildcards (`*` in place of the first letter, which the server does not match against the list): first every other word of the artist and of the title, or of your `query_text`, starting with the first (`*lla Fitzgerald *inter Wonderland`), then the others. A word becomes a wildcard only when it starts with a letter or digit and another letter or digit follows, so never `7` or `E.`, and a text of three words or more keeps one plain word, since peers answer junk to wildcards alone. Each search that gets no answer takes about 15 seconds. Discovery stops searching for a track whose search found nothing in 3 runs in a row.

Ratings, discovery and playlists:

22. A connected app's rating is refused when it could lead to a deletion later, whatever its switch and your settings: a 1-star song rating, and any rating of a pending discovery track other than clearing it. No app can change the discovery folder or the promote threshold. While you have pending discovery tracks, a rating also needs Soulbeet to find the song's file, and is refused when it cannot (the music folder or `NAVIDROME_MUSIC_PATH` is wrong).
23. Navidrome's scanner can give a newly staged discovery file the id and rating of a missing library song. An app's earlier 5-star rating of that song then reads as the track's, and the next sync, yours or the 6-hourly one, promotes the track and replaces any earlier copy as Soulbeet's own deletion, outside the app's daily limit. This is rare and not closed.
24. A batch whose playlist could not begin, and whose removal then failed, shows in `list_batches` until it is pruned, 30 days after it was made.

The delete tools:

25. A song added or removed while `find_duplicates` reads, or with Navidrome's `RecentlyAddedByModTime` a file that changes, can shift Navidrome's pages, and the search then fails with `navidrome_unreachable` ("Navidrome counts {total} songs, and its pages held {read}. Search again."). A song removed while it reads can let another pass unseen, so search again after deleting.
26. A deletion whose task stopped before it reported is recorded outside the day's count, so an app's count can reach 101 once.
27. `replace_existing` replaces only for track items whose previewed file is inside the folder the jobs import into. A previewed copy beets finds no duplicate of stays and is listed in `not_replaced`, as is every copy a job did not replace within its 24 hours.
28. `preview_delete_tracks`, `delete_tracks` and `get_deletion_history` show the absolute paths of the files, which are in the user's own folders.

Proxies and MCP clients:

29. A tool call waits at most 30 seconds for running work, since claude.ai was seen to drop a call after 90 seconds and many proxies end a silent answer after 60. The assistant follows long work by calling again.
30. `get_tag_tracks` (tracks of a genre or tag from ListenBrainz) is not built.

## Security

What exposing Soulbeet to MCP clients adds:

- The OAuth endpoints and `/mcp` answer only while the Security Checks pass, and only over https or on a loopback address. `MCP=off` turns them all off.
- Access and refresh tokens are opaque, stored only as hashes, and bound to the address they were issued for.
- Each token belongs to one user, whose account is loaded again on every call: a disconnected app, a changed password or a deleted account stops it at once.
- No tool takes a file path or returns a credential. A tool shows the user's own data, and other users' public playlists as Navidrome lists them; a deletion refusal never names the listener who kept the track.
- A deletion needs the owner's switch, a preview, and a second call with its single-use token, bound to that user and that app.
- No limit is keyed by the client's IP address. Put any IP rules in the proxy.
- Keep the admin password and `SECRET_KEY` safe, and back up `secret.key` with the database.

## Upgrading

The release that adds MCP access changes these on the first start:

- `MCP` is on unless you set it off, but the endpoints stay closed until the Security Checks in Settings > Config pass.
- A `SECRET_KEY` under 32 bytes is no longer used, nor the key built into earlier versions for installs without `SECRET_KEY`: Soulbeet signs sessions with the key it generates in `secret.key`. Everyone is signed out once; stored Navidrome passwords are stored again under the new key on that start.
- `NAVIDROME_MUSIC_PATH` now names where Soulbeet sees the root of Navidrome's default library, not Navidrome's own path. If you set it for an earlier version, change it to the path inside the Soulbeet container, or remove it. While it names no folder Soulbeet can see, the startup log and Settings > Library > Deletion Checks say so, and every deletion in that library is refused with `delete_path_unresolved`.
- The web page's download updates socket opens only from a page at the address the request was sent to or at `PUBLIC_URL`, so a reverse proxy must pass the public name in `Host` or `X-Forwarded-Host`, or `PUBLIC_URL` must be set.
- A browser tab left open across the upgrade cannot read a Deletion History row of reason `cleanup` until it reloads.
- The checks' messages no longer name file paths, in Settings > Library ("Kept by auto-delete") and in import notes.
- Turning on **Allow deleting music** for an app asks first, with the text quoted under [Connected apps](#connected-apps).

The release that removes Last.fm changes the tools:

- `search_music` no longer takes `provider`, and a call that passes it is refused with `invalid_input`. Its answer no longer has `provider_requested` or `provider_used`, nor has `get_release`'s.
- `get_release` takes a MusicBrainz release id only; a `lastfm:` id is refused with `invalid_input`.
- `get_settings` no longer has `default_metadata_provider` or `lastfm`, and has `discovery.deezer_recommendations`.
- `get_discovery` has `last_run`, the newest recommendation run with its sources and failed sources.
- The failure code `provider_not_configured`, the user action `configure_provider` and the cause service `lastfm` are gone.
- A job whose album came from a Last.fm search and has no track list yet fails with `metadata_not_found`: search for the album again.
