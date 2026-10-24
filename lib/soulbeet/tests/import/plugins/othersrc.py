"""A second metadata source that answers every id lookup, the way deezer and
tidal answer an MBID whose leading digits fit their id pattern."""

from beets.autotag.hooks import AlbumInfo, TrackInfo
from beets.metadata_plugins import MetadataSourcePlugin


def _track(track_id, i=None):
    return TrackInfo(
        title=f"Other Source Song {i or 1}",
        artist="Other Source Artist",
        track_id=track_id if i is None else f"{track_id}-{i}",
        index=i,
        medium=1 if i else None,
        medium_index=i,
        data_source="OtherSrc",
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
        return []
