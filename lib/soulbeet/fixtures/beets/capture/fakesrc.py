"""Offline metadata source for the classifier fixtures.

FAKESRC_MODE picks what every lookup returns:
  strong  one exact candidate (beets applies it)
  weak    three candidates that match badly (beets skips or imports as-is)
  none    no candidates
  error   raises, so beets logs "Error in 'FakeSrc.<method>': ..."
An artist containing "Weakband" is always weak, so one run can mix outcomes.
FAKESRC_CRASH=1 adds an import stage that raises, an uncaught pipeline crash.
"""

import os

from beets.autotag.hooks import AlbumInfo, TrackInfo
from beets.metadata_plugins import MetadataSourcePlugin


def _mode(artist):
    if "Weakband" in (artist or ""):
        return "weak"
    return os.environ.get("FAKESRC_MODE", "none")


def _tracks(items, garble):
    return [
        TrackInfo(
            title=f"Totally Different {i}" if garble else it.title,
            track_id=f"fake-t-{i}{'g' if garble else ''}",
            artist="Some Other Artist" if garble else it.artist,
            length=it.length + (200 if garble else 0),
            index=i,
            medium=1,
            medium_index=i,
            medium_total=len(items),
            data_source="FakeSrc",
        )
        for i, it in enumerate(items, 1)
    ]


class FakeSrcPlugin(MetadataSourcePlugin):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        if os.environ.get("FAKESRC_CRASH"):

            def crash(session, task):
                raise TypeError("simulated crash in import stage")

            self.import_stages = [crash]

    def album_for_id(self, album_id):
        return None

    def track_for_id(self, track_id):
        return None

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
            return [
                AlbumInfo(
                    _tracks(items, True),
                    album=f"Other Album {k}",
                    album_id=f"fake-a-w{k}",
                    artist="Some Other Artist",
                    artist_id="fake-ar-2",
                    data_source="FakeSrc",
                    mediums=1,
                )
                for k in range(3)
            ]
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
            return [
                TrackInfo(
                    title=f"Totally Different {k}",
                    artist="Some Other Artist",
                    track_id=f"fake-t-w{k}",
                    length=item.length + 200,
                    data_source="FakeSrc",
                )
                for k in range(3)
            ]
        return []
