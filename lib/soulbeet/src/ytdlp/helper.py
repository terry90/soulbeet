"""Soulbeet's YouTube Music helper.

Soulbeet runs it as `python -I -X utf8 -c <this file>`, with no shell and an environment that
holds only its own folders, and writes one request as JSON on stdin. The helper answers with one
JSON object per line on stdout: progress lines while it downloads, then one answer or one error.
Nothing else reaches stdout: yt-dlp's own output, and every program it runs, write to stderr.
"""

import importlib
import importlib.metadata
import json
import os
import platform
import re
import subprocess
import sys
import time

URL = re.compile(r"https?://\S+")
VIDEO_ID = re.compile(r"^[A-Za-z0-9_-]{11}$")
BROWSE_ID = re.compile(r"^[A-Za-z0-9_-]{1,64}$")
MESSAGE_MAX = 500
# A program asked for its version gets this long.
VERSION_SECONDS = 15
# Progress lines are sent at most this often while bytes come in.
PROGRESS_EVERY = 0.5


def clean(text):
    """`text` without URLs: a signed media URL carries the server's IP address."""
    return URL.sub("<url>", str(text)).strip()[:MESSAGE_MAX]


class Failed(Exception):
    """A failure the helper names itself."""

    def __init__(self, kind, message):
        super().__init__(message)
        self.kind = kind


def kind_of(error):
    """The kind of failure `error` is, from its type and yt-dlp's or ytmusicapi's words."""
    if isinstance(error, Failed):
        return error.kind
    if isinstance(error, ImportError):
        return "tooling"
    name = type(error).__name__
    text = f"{name}: {error}".lower()
    if name == "GeoRestrictedError" or any(
        s in text for s in ("confirm your age", "age-restricted", "inappropriate for some users")
    ):
        return "unavailable"
    if any(s in text for s in ("try again later", "rate-limited", "http error 429", "too many requests")):
        return "rate_limited"
    if any(s in text for s in ("not a bot", "sign in to confirm", "captcha")):
        return "bot_check"
    if "requested format is not available" in text:
        return "format_missing"
    if "http error 403" in text or "403: forbidden" in text:
        return "forbidden"
    if any(
        s in text
        for s in (
            "video unavailable",
            "private video",
            "has been removed",
            "not available in your country",
            "this video is not available",
            "isn't available",
        )
    ):
        return "unavailable"
    if isinstance(error, (TimeoutError, ConnectionError)) or any(
        s in text
        for s in (
            "timed out",
            "timeout",
            "bytes read",
            "incompleteread",
            "connection reset",
            "connection refused",
            "connection aborted",
            "connectionerror",
            "remote end closed",
            "name resolution",
            "network is unreachable",
            "transporterror",
        )
    ):
        return "network"
    if "ffmpeg" in text and ("not found" in text or "not installed" in text):
        return "tooling"
    if "javascript runtime" in text:
        return "tooling"
    return "internal"


def names(artists):
    return [a["name"] for a in artists or [] if isinstance(a, dict) and a.get("name")]


def ytmusic():
    from ytmusicapi import YTMusic

    return YTMusic()


def program_version(args, pattern):
    """The version `args` prints on its first line, or None when it does not run."""
    try:
        ran = subprocess.run(
            args,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=VERSION_SECONDS,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if ran.returncode != 0:
        return None
    found = re.search(pattern, (ran.stdout or "").partition("\n")[0])
    return found.group(1) if found else None


def version(request, emit):
    found = {"python": platform.python_version()}
    for key, module, dist in (
        ("yt_dlp", "yt_dlp", "yt-dlp"),
        ("ejs", "yt_dlp_ejs", "yt-dlp-ejs"),
        ("ytmusicapi", "ytmusicapi", "ytmusicapi"),
    ):
        try:
            importlib.import_module(module)
            found[key] = importlib.metadata.version(dist)
        except Exception:
            found[key] = None
    found["deno"] = program_version([request["deno"], "--version"], r"^deno (\S+)")
    ffmpeg = request.get("ffmpeg")
    found["ffmpeg"] = program_version([ffmpeg, "-version"], r"^ffmpeg version (\S+)") if ffmpeg else None
    return {"version": found}


def search_albums(request, emit):
    found = ytmusic().search(request["query"], filter="albums", limit=request["limit"])
    albums = [
        {
            "browse_id": a["browseId"],
            "title": a.get("title") or "",
            "artists": names(a.get("artists")),
            "year": a.get("year"),
            "explicit": bool(a.get("isExplicit")),
        }
        for a in found
        if a.get("browseId")
    ]
    return {"albums": albums[: request["limit"]]}


def album(request, emit):
    browse_id = request["browse_id"]
    if not BROWSE_ID.match(browse_id):
        raise Failed("internal", f"{browse_id!r} is not a browse id")
    page = ytmusic().get_album(browse_id)
    tracks = [
        {
            "video_id": t.get("videoId"),
            "title": t.get("title") or "",
            "track_number": t.get("trackNumber"),
            "duration_s": t.get("duration_seconds"),
            "available": bool(t.get("isAvailable")),
            "video_type": t.get("videoType"),
            "artists": names(t.get("artists")),
            "explicit": bool(t.get("isExplicit")),
        }
        for t in page.get("tracks") or []
    ]
    return {
        "album": {
            "title": page.get("title") or "",
            "artists": names(page.get("artists")),
            "year": page.get("year"),
            "tracks": tracks,
        }
    }


def search_songs(request, emit):
    found = ytmusic().search(request["query"], filter="songs", limit=request["limit"])
    songs = []
    for s in found:
        if not s.get("videoId"):
            continue
        on = s.get("album") or {}
        songs.append(
            {
                "video_id": s["videoId"],
                "title": s.get("title") or "",
                "artists": names(s.get("artists")),
                "album": {"name": on["name"], "browse_id": on.get("id")} if on.get("name") else None,
                "duration_s": s.get("duration_seconds"),
                "available": s.get("isAvailable", True) is not False,
                "video_type": s.get("videoType"),
                "explicit": bool(s.get("isExplicit")),
            }
        )
    return {"songs": songs[: request["limit"]]}


class StderrLogger:
    """yt-dlp's messages, on stderr only."""

    def debug(self, message):
        pass

    def info(self, message):
        pass

    def warning(self, message):
        sys.stderr.write(clean(message) + "\n")

    def error(self, message):
        sys.stderr.write(clean(message) + "\n")


def runnable(path, what):
    if not path or not os.path.isfile(path) or not os.access(path, os.X_OK):
        raise Failed("tooling", f"{what} is not at {path}")


def options(request, staging):
    """What every yt-dlp run here is: one YouTube video, quiet, sandboxed JS, paced."""
    runnable(request.get("deno"), "deno")
    runnable(request.get("ffmpeg"), "ffmpeg")
    return {
        "quiet": True,
        "noprogress": True,
        "noplaylist": True,
        "logger": StderrLogger(),
        "allowed_extractors": ["youtube"],
        "cachedir": request["cache_dir"],
        "paths": {"home": staging, "temp": staging},
        "outtmpl": {"default": "%(id)s.%(ext)s"},
        "ffmpeg_location": request["ffmpeg"],
        "js_runtimes": {"deno": {"path": request["deno"]}},
        "sleep_interval_requests": 0.75,
        "socket_timeout": 30,
        "retries": 3,
        "fragment_retries": 3,
        "overwrites": True,
        "updatetime": False,
        "writethumbnail": False,
    }


def watch_url(video_id):
    if not VIDEO_ID.match(video_id):
        raise Failed("internal", f"{video_id!r} is not a video id")
    return f"https://music.youtube.com/watch?v={video_id}"


def fetch(request, emit):
    url = watch_url(request["video_id"])
    staging = request["staging_dir"]
    import yt_dlp

    sent = [0.0]

    def hook(d):
        status = d.get("status")
        now = time.monotonic()
        if status not in ("downloading", "finished"):
            return
        if status == "downloading" and now - sent[0] < PROGRESS_EVERY:
            return
        sent[0] = now
        total = d.get("total_bytes") or d.get("total_bytes_estimate")
        emit(
            {
                "progress": {
                    "downloaded": int(d.get("downloaded_bytes") or 0),
                    "total": int(total) if total else None,
                    "speed": d.get("speed"),
                }
            }
        )

    run = options(request, staging)
    run["format"] = f"bestaudio[acodec=opus][abr>={int(request['min_abr'])}]"
    # The source is Opus already, so ffmpeg copies the stream out of its WebM.
    run["postprocessors"] = [{"key": "FFmpegExtractAudio", "preferredcodec": "opus"}]
    run["progress_hooks"] = [hook]
    with yt_dlp.YoutubeDL(run) as ydl:
        info = ydl.extract_info(url, download=True)
    downloads = info.get("requested_downloads") or [{}]
    path = downloads[0].get("filepath")
    if not path:
        raise Failed("internal", "yt-dlp named no file")
    return {
        "done": {
            "path": path,
            "format_id": str(info.get("format_id") or ""),
            "acodec": str(info.get("acodec") or ""),
            "abr": info.get("abr"),
            "asr": info.get("asr"),
            "duration": info.get("duration"),
        }
    }


def probe(request, emit):
    url = watch_url(request["video_id"])
    import yt_dlp

    run = options(request, request["staging_dir"])
    run["format"] = "bestaudio[acodec=opus]"
    # yt-dlp's own test mode: the first 10 KB only.
    run["test"] = True
    with yt_dlp.YoutubeDL(run) as ydl:
        ydl.extract_info(url, download=True)
    return {"probe": "ok"}


COMMANDS = {
    "version": version,
    "search_albums": search_albums,
    "album": album,
    "search_songs": search_songs,
    "fetch": fetch,
    "probe": probe,
}


def main():
    answers = os.fdopen(os.dup(sys.stdout.fileno()), "w", encoding="utf-8")
    # From here on fd 1, sys.stdout and every program run write to stderr.
    os.dup2(sys.stderr.fileno(), sys.stdout.fileno())
    sys.stdout = sys.stderr

    def emit(answer):
        answers.write(json.dumps(answer, ensure_ascii=False) + "\n")
        answers.flush()

    try:
        request = json.loads(sys.stdin.readline())
        sys.path.insert(0, request["site_packages"])
        command = COMMANDS.get(request.get("command"))
        if command is None:
            raise Failed("internal", f"Unknown command {request.get('command')!r}")
        emit(command(request, emit))
    except Exception as error:
        named = str(error) if isinstance(error, Failed) else f"{type(error).__name__}: {error}"
        emit({"error": {"kind": kind_of(error), "message": clean(named)}})


main()
