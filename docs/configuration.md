# Configuration

This page lists the environment variables Soulbeet reads and the settings kept in the web UI.

## Environment variables

| Variable | Description | Default |
|----------|-------------|---------|
| `DATABASE_URL` | Connection string for SQLite. The folder of the database file also holds `secret.key` and `beets-libraries`, the beets library database of each music folder (see [Library databases](beets.md#library-databases)). Keep it on a local disk: SQLite cannot lock files on most network shares | `sqlite:soulbeet.db` |
| `DOWNLOAD_PATH` | Path where slskd saves downloads | `/downloads` |
| `SECRET_KEY` | Signs sessions and encrypts stored Navidrome passwords. Optional: when unset, a key is generated on the first start and stored in `secret.key` next to the database, where it is read on later starts. A key you set of at least 32 bytes (`openssl rand -hex 32`) wins over that file. A shorter key is not used: Soulbeet signs with the generated key instead, and Settings > Config lists it under Security Checks until you remove it or set a longer one. Changing the key signs everyone out and makes stored Navidrome passwords unreadable until each user enters theirs again, by signing in with it or under Connect Navidrome in Settings > Users | generated |
| `NAVIDROME_URL` | Your Navidrome server URL. Optional: without it you log in with local accounts, and discovery playlists, rating sync and auto-delete are unavailable | |
| `NAVIDROME_PUBLIC_URL` | Optional. Externally reachable Navidrome URL advertised to browser and native clients via `/api/client/bootstrap` when `NAVIDROME_URL` is a container-internal address (for example `http://navidrome:4533`). Falls back to `NAVIDROME_URL` when unset | |
| `REGISTRATION` | Who can create accounts: `navidrome` (accounts appear when a Navidrome user signs in), `admin` (only an admin creates accounts), `open` (anyone can register) | `navidrome` with `NAVIDROME_URL`, else `admin` |
| `BEETS_CONFIG` | Path to your beets config file. Replaces the shipped `/app/beets_config.yaml` (beets defaults still apply underneath) | `beets_config.yaml` |
| `BEETS_PYTHON` | Python interpreter that runs beets imports and the config check. By default it is read from the `#!` line of `beet` (`/usr/bin/python3` in the image) | |
| `PUBLIC_URL` | Optional. The public address (`https://host[:port]`, no path). When set, Soulbeet uses it instead of reading the address from request headers | |
| `MCP` | `on` or `off`, in any case, for the OAuth and `/mcp` endpoints MCP clients use; any other value turns them off and logs a warning | `on` (unset) |
| `NAVIDROME_MUSIC_PATH` | Optional. Where Soulbeet sees the root of Navidrome's default library. Set it when Soulbeet mounts that library at another path than Navidrome does (e.g. Navidrome at `/music`, Soulbeet at `/media/music`): deletion checks, the rating sync and the MCP tools use it to find Navidrome's songs on disk. Without it, Soulbeet learns where each library is from the files it imports | |
| `BEETS_ALBUM_MODE` | Enable album import mode (see [Album mode](beets.md#album-mode-beets_album_mode)) | `false` |
| `RUST_LOG` | Log filter. Use `info,soulbeet::beets::output=debug` to log every line beets prints. A bare `soulbeet::beets::output=debug` turns off all other info logs | |

The `OIDC_*` variables for single sign-on are listed in [sso.md](sso.md#settings).

## Signing in

For an account linked to Navidrome, Navidrome decides: if it rejects your credentials, the sign-in is refused. Only while Navidrome cannot be reached (or `NAVIDROME_URL` is not set) does Soulbeet check the password stored in its own database, which follows your Navidrome password after each successful Navidrome login. Accounts not linked to Navidrome sign in with their Soulbeet password. A Navidrome login only takes over an existing Soulbeet account that is already linked to Navidrome or whose Soulbeet password is the same.

Usernames are unique without regard to letter case, so `Bob` and `bob` are the same name.

## Settings in the web UI

slskd URL and API key are configured through the web UI (Settings > Config) and stored in the database. The ListenBrainz username, the optional token and the Deezer switch are configured per-user in Settings > Library.

## Download sources

Soulseek, through slskd, is always on. Every other download source is off until an admin turns it on in Settings > Config > Connections, under the slskd settings; turning one on asks first. A source that is on is searched by auto-download next to Soulseek, and the status line under the search bar has a row for it. A source whose files are lossy only, YouTube Music, is searched only when no other source has a match auto-download would take.

Turning a source off stops its searches and its unfinished downloads and deletes their partial files. Downloads still waiting on it fail with "{name} is turned off in Settings > Config > Connections". Files it finished stay where they are.

### Internet Archive

Downloads original files that uploaders gave archive.org, each checked against archive.org's md5 before it is imported; see [archive.md](archive.md) for what it offers and how it behaves. It needs no account and no address. Partial files sit in `DOWNLOAD_PATH/.soulful-partial/`, so `DOWNLOAD_PATH` must be writable by Soulbeet and have room for the files being downloaded. `ARCHIVE_HOST` (`host:port`, reached over http) points it at a test double instead of archive.org; leave it unset.

### YouTube Music

Searches YouTube Music and downloads Opus audio at about 130 kbps through yt-dlp, anonymously; see [ytdlp.md](ytdlp.md). YouTube's Terms of Service forbid downloading from it: turning it on is the admin's choice. Only the `:full` image has the tools it needs; in another image it says "Not in this image: use the :full image" on its status line and downloads nothing. Each user chooses in Settings > Library what auto-download does when only YouTube Music has an album: never use it, ask (the default), or download automatically. Its staging files sit in `DOWNLOAD_PATH/.soulful-partial/`, like the Internet Archive's.

The paths of its tools, which the `:full` image sets up; change them only to run the helper elsewhere:

| Variable | Description | Default |
| --- | --- | --- |
| `YTDLP_PYTHON` | The Python 3.11 or later that runs the helper | `/usr/bin/python3` |
| `YTDLP_SITE_PACKAGES` | The folder that holds yt-dlp, yt-dlp-ejs and ytmusicapi, apart from beets' packages | `/opt/ytdlp/lib/python3.11/site-packages` |
| `YTDLP_DENO` | The deno that runs YouTube's player code for yt-dlp, with no permissions | `/opt/ytdlp/bin/deno` |

ffmpeg is the one on `PATH` (`/usr/local/bin/ffmpeg` in `:full`). A relative path in one of these variables, or in `DOWNLOAD_PATH`, is read from the folder Soulbeet was started in.

### NetEase (GD Studio)

Downloads NetEase's files through GD Studio's public API, each checked against the size GD Studio gives and the MD5 NetEase names it by, then tagged with the track you asked for; see [gdstudio.md](gdstudio.md). GD Studio's API page forbids downloading; read that page before you turn this on. It needs no account and no address. Soulbeet sends the API at most 45 calls in 5 minutes for every user together. The container needs outbound https to `music-api.gdstudio.xyz` and to NetEase's file servers (`*.music.126.net`). Partial files sit in `DOWNLOAD_PATH/.soulful-partial/`, as for the Internet Archive. `GDSTUDIO_HOST` (`host:port`, reached over http) points it at a test double instead of GD Studio; leave it unset.

### Direct Connect

Searches the Direct Connect hubs an AirDC++ Web Client you run is connected to, and has it download the files, each checked by AirDC++ against its TTH before it is imported; see [direct-connect.md](direct-connect.md) for setting AirDC++ up. Under its switch, give the AirDC++ address (for example `http://airdcpp:5600`, with no user name or password in it; `http://` only on a private container network or the same machine, since the password goes with every request, `https://` to reach another host), the web user Soulbeet signs in as and its password, then **Save Direct Connect**. The password is stored encrypted under `SECRET_KEY` and never shown again; saving with the password field empty keeps it. **Check AirDC++** shows what Soulbeet sees of it. AirDC++ must save into the same folder as `DOWNLOAD_PATH`.

How a download source plugs in is in [download-backends.md](download-backends.md).
