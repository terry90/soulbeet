"""Stands in for yt-dlp: downloads the Creative Commons track MdzF0rFOC9Y as yt-dlp did on
2026-11-25 (../../answers/monkeys-fetch.json), and fails the other video ids of FAILURES with
yt-dlp's own words. It writes the options it was given next to the file it fetched."""

import json
import os
from pathlib import Path

ANSWERS = Path(__file__).resolve().parents[2] / "answers"

# yt-dlp's messages, from extractor/youtube/_video.py, YoutubeDL.py and networking.
FAILURES = {
    "RATELIMIT01": "ERROR: [youtube] RATELIMIT01: This content isn't available, try again later. "
    "The current session has been rate-limited by YouTube for up to an hour. It is recommended to "
    "use `-t sleep` to add a delay between video requests to avoid exceeding the rate limit. For "
    "more information, refer to  https://github.com/yt-dlp/yt-dlp/wiki/Extractors#this-content-isnt-available-try-again-later",
    "BOTCHECK001": "ERROR: [youtube] BOTCHECK001: Sign in to confirm you’re not a bot. Use "
    "--cookies-from-browser or --cookies for the authentication. See  "
    "https://github.com/yt-dlp/yt-dlp/wiki/FAQ#how-do-i-pass-cookies-to-yt-dlp  for how to "
    "manually pass cookies.",
    "AGECHECK001": "ERROR: [youtube] AGECHECK001: Sign in to confirm your age. This video may be "
    "inappropriate for some users.",
    "FORBIDDEN01": "ERROR: unable to download video data: HTTP Error 403: Forbidden",
    "GONEVIDEO01": "ERROR: [youtube] GONEVIDEO01: Video unavailable. This video is no longer "
    "available because the YouTube account associated with this video has been terminated.",
    "NOFORMAT001": "ERROR: [youtube] NOFORMAT001: Requested format is not available. Use "
    "--list-formats for a list of available formats",
    "NETWORK0001": "ERROR: unable to download video data: <urlopen error [Errno -3] Temporary "
    "failure in name resolution>",
    "CUTSHORT001": "ERROR: ('Connection broken: IncompleteRead(1024 bytes read, 2047 more "
    "expected)', IncompleteRead(1024 bytes read, 2047 more expected))",
    "STRANGE0001": "ERROR: [youtube] STRANGE0001: KeyError('streamingData'); please report this "
    "issue on  https://github.com/yt-dlp/yt-dlp/issues?q= ",
}


class DownloadError(Exception):
    """As yt_dlp.utils.DownloadError."""


class YoutubeDL:
    def __init__(self, params):
        self.params = params

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False

    def extract_info(self, url, download=True):
        video_id = url.rsplit("=", 1)[-1]
        if video_id in FAILURES:
            raise DownloadError(FAILURES[video_id])
        saved = json.loads((ANSWERS / "monkeys-fetch.json").read_text())
        home = self.params["paths"]["home"]
        kept = {k: v for k, v in self.params.items() if k not in ("logger", "progress_hooks")}
        Path(home, "options.json").write_text(json.dumps(kept))
        for event in saved["events"]:
            for hook in self.params.get("progress_hooks", []):
                hook(dict(event))
        info = dict(saved["info"], id=video_id)
        if self.params.get("postprocessors"):
            info["ext"] = "opus"
        path = os.path.join(home, f"{video_id}.{info['ext']}")
        Path(path).write_bytes(b"OggS fake opus")
        info["requested_downloads"] = [{"filepath": path}]
        return info
