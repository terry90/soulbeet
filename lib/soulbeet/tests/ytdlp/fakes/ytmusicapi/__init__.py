"""Stands in for ytmusicapi: answers the searches and albums the tests ask with the answers
YouTube Music gave on 2026-11-25, saved in ../../answers. Any other request finds nothing, and a
query of "fail: <text>" fails as ytmusicapi does when YouTube Music answers with an error."""

import json
from pathlib import Path

ANSWERS = Path(__file__).resolve().parents[2] / "answers"

SEARCHES = {
    ("Slint Spiderland", "albums"): "slint-albums.json",
    ("Slint Breadcrumb Trail", "songs"): "slint-songs.json",
    ("Kevin MacLeod Monkeys Spinning Monkeys", "songs"): "monkeys-songs.json",
    ("Kendrick Lamar luther", "songs"): "luther-songs.json",
}
ALBUMS = {"MPREb_ur8iGYISD11": "slint-spiderland.json"}


class YTMusicServerError(Exception):
    """As ytmusicapi.exceptions.YTMusicServerError."""


def saved(name):
    return json.loads((ANSWERS / name).read_text())


class YTMusic:
    def search(self, query, filter=None, limit=20):
        if query.startswith("fail: "):
            raise YTMusicServerError(query[len("fail: "):])
        name = SEARCHES.get((query, filter))
        return saved(name) if name else []

    def get_album(self, browseId):
        return saved(ALBUMS[browseId])
