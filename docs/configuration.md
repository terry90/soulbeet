# Configuration

This page lists the environment variables Soulbeet reads and the settings kept in the web UI.

## Environment variables

| Variable | Description | Default |
|----------|-------------|---------|
| `DATABASE_URL` | Connection string for SQLite. The folder of the database file also holds `secret.key` and `beets-libraries`, the beets library database of each music folder (see [Library databases](beets.md#library-databases)). Keep it on a local disk: SQLite cannot lock files on most network shares | `sqlite:soulbeet.db` |
| `DOWNLOAD_PATH` | Path where slskd saves downloads | `/downloads` |
| `SECRET_KEY` | Signs sessions and encrypts stored Navidrome passwords. Optional: when unset, a key is generated on the first start and stored in `secret.key` next to the database, where it is read on later starts. A key you set of at least 32 bytes (`openssl rand -hex 32`) wins over that file. A shorter key is not used: Soulbeet signs with the generated key instead, and Settings > Config lists it under Security Checks until you remove it or set a longer one. Changing the key signs everyone out and makes stored Navidrome passwords unreadable until each user enters theirs again, by signing in with it or under Connect Navidrome in Settings > Users | generated |
| `NAVIDROME_URL` | Your Navidrome server URL. Optional: without it you log in with local accounts, and discovery playlists, rating sync and auto-delete are unavailable | |
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
