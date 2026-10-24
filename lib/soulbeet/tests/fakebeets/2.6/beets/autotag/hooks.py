class AttrDict(dict):
    """Before 2.8, an id was only a key: reading info.id raised AttributeError."""

    def __getattr__(self, key):
        try:
            return self[key]
        except KeyError:
            raise AttributeError(key) from None


class AlbumInfo(AttrDict):
    pass


class TrackInfo(AttrDict):
    pass
