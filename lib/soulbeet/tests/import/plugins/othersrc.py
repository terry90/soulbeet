"""A second metadata source that answers every id lookup, the way deezer and
tidal answer an MBID whose leading digits fit their id pattern.

OTHERSRC_ARTIST, when set, makes track searches return one track by that
artist. OTHERSRC_ODD_IDS adds three tracks: one with a numeric id, one
with none and one with no data source."""

import os

from beets.autotag.hooks import AlbumInfo, TrackInfo
from beets.metadata_plugins import MetadataSourcePlugin


def _track(track_id, i=None, artist="Other Source Artist", data_source="OtherSrc"):
    return TrackInfo(
        title=f"Other Source Song {i or 1}",
        artist=artist,
        track_id=track_id if i is None else f"{track_id}-{i}",
        index=i,
        medium=1 if i else None,
        medium_index=i,
        data_source=data_source,
    )


class OtherSrcPlugin(MetadataSourcePlugin):
    def album_for_id(self, album_id):
        return AlbumInfo(
            [_track(album_id, i) for i in (1, 2)],
            album="Other Source Album",
            album_id=album_id,
            artist="Other Source Artist",
            data_source="OtherSrc",
            mediums=1,
        )

    def track_for_id(self, track_id):
        return _track(track_id)

    def candidates(self, items, artist, album, va_likely):
        return []

    def item_candidates(self, item, artist, title):
        found = []
        if artist_name := os.environ.get("OTHERSRC_ARTIST"):
            found.append(_track("other-t-1", artist=artist_name))
        if os.environ.get("OTHERSRC_ODD_IDS"):
            found += [
                _track(3135556),
                _track(None),
                _track("no-source", data_source=None),
            ]
        return found
