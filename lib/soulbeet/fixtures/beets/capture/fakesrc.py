"""Offline metadata source for the classifier fixtures.

FAKESRC_MODE picks what every lookup returns:
  strong  one exact candidate (beets applies it)
  weak    three candidates that match badly (beets skips or imports as-is)
  none    no candidates
  error   raises, so beets logs "Error in 'FakeSrc.<method>': ..."
An artist containing "Weakband" is always weak, so one run can mix outcomes.
FAKESRC_CRASH=1 adds an import stage that raises, an uncaught pipeline crash.

Lookups by id (`-S`) serve the weak candidates in any mode. Without the items
to build from, a weak album has LOOKUP_TRACKS tracks and nothing has a length.
"""

import os

from beets.autotag.hooks import AlbumInfo, TrackInfo
from beets.metadata_plugins import MetadataSourcePlugin

WEAK_COUNT = 3
LOOKUP_TRACKS = 2


def _mode(artist):
    if "Weakband" in (artist or ""):
        return "weak"
    return os.environ.get("FAKESRC_MODE", "none")


def _album_track(i, total, title, artist, length, track_id):
    return TrackInfo(
        title=title,
        track_id=track_id,
        artist=artist,
        length=length,
        index=i,
        medium=1,
        medium_index=i,
        medium_total=total,
        data_source="FakeSrc",
    )


def _weak_album_track(i, total, length):
    return _album_track(
        i, total, f"Totally Different {i}", "Some Other Artist", length, f"fake-t-{i}g"
    )


def _tracks(items, garble):
    total = len(items)
    if garble:
        return [
            _weak_album_track(i, total, it.length + 200)
            for i, it in enumerate(items, 1)
        ]
    return [
        _album_track(i, total, it.title, it.artist, it.length, f"fake-t-{i}")
        for i, it in enumerate(items, 1)
    ]


def _weak_album(k, tracks):
    return AlbumInfo(
        tracks,
        album=f"Other Album {k}",
        album_id=f"fake-a-w{k}",
        artist="Some Other Artist",
        artist_id="fake-ar-2",
        data_source="FakeSrc",
        mediums=1,
    )


def _weak_track(k, length):
    return TrackInfo(
        title=f"Totally Different {k}",
        artist="Some Other Artist",
        track_id=f"fake-t-w{k}",
        length=length,
        data_source="FakeSrc",
    )


def _weak_index(id_, prefix):
    """k for an id `<prefix>k` naming one of the weak candidates, else None."""
    if id_.startswith(prefix) and id_[len(prefix):].isdigit():
        k = int(id_[len(prefix):])
        if k < WEAK_COUNT:
            return k
    return None


class FakeSrcPlugin(MetadataSourcePlugin):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        if os.environ.get("FAKESRC_CRASH"):

            def crash(session, task):
                raise TypeError("simulated crash in import stage")

            self.import_stages = [crash]

    def album_for_id(self, album_id):
        k = _weak_index(album_id, "fake-a-w")
        if k is None:
            return None
        tracks = [
            _weak_album_track(i, LOOKUP_TRACKS, None)
            for i in range(1, LOOKUP_TRACKS + 1)
        ]
        return _weak_album(k, tracks)

    def track_for_id(self, track_id):
        k = _weak_index(track_id, "fake-t-w")
        return None if k is None else _weak_track(k, None)

    def candidates(self, items, artist, album, va_likely):
        mode = _mode(artist)
        if mode == "error":
            raise RuntimeError("simulated network failure")
        if mode == "strong":
            return [
                AlbumInfo(
                    _tracks(items, False),
                    album=album,
                    album_id="fake-a-1",
                    artist=artist,
                    artist_id="fake-ar-1",
                    data_source="FakeSrc",
                    mediums=1,
                )
            ]
        if mode == "weak":
            return [_weak_album(k, _tracks(items, True)) for k in range(WEAK_COUNT)]
        return []

    def item_candidates(self, item, artist, title):
        mode = _mode(artist)
        if mode == "error":
            raise RuntimeError("simulated network failure")
        if mode == "strong":
            return [
                TrackInfo(
                    title=item.title,
                    artist=item.artist,
                    track_id=f"fake-t-s-{item.title}",
                    length=item.length,
                    data_source="FakeSrc",
                )
            ]
        if mode == "weak":
            return [_weak_track(k, item.length + 200) for k in range(WEAK_COUNT)]
        return []
