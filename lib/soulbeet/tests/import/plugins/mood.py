"""A media field of a plugin's own, typed as text: MOOD, read from and
written to files like the fields beets ships."""

from beets.dbcore import types
from beets.plugins import BeetsPlugin
from mediafile import MediaField, MP3DescStorageStyle, MP4StorageStyle, StorageStyle


class MoodPlugin(BeetsPlugin):
    item_types = {"mood": types.STRING}

    def __init__(self):
        super().__init__()
        self.add_media_field(
            "mood",
            MediaField(
                MP3DescStorageStyle("MOOD"),
                MP4StorageStyle("----:com.apple.iTunes:MOOD"),
                StorageStyle("MOOD"),
            ),
        )
