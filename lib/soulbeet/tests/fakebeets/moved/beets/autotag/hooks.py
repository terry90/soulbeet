class Info(dict):
    @property
    def id(self):
        return self.get("id")


class AlbumInfo(Info):
    pass


class TrackInfo(Info):
    pass
