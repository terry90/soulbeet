"""Two-pass MusicBrainz track search.

beets sends `alias: <title>` alongside `recording: <title>` when it searches
for a single track. That term does not narrow the search, it adds tokens to
it, and MusicBrainz scores a recording higher the more often it repeats them.
A short, common title loses to a long repetitive one: searching for Kowloon's
"Wake Up" returns "Up Up Up Up Up Up" ahead of the exact recording, which is
then far enough away that a quiet import skips the file with no explanation.

Dropping the alias term outright would cost the case it exists for, recordings
whose canonical MusicBrainz title differs from the file's. So this plugin
searches without it first and only falls back to the alias query when nothing
came back close enough to import. The common case gets an undiluted query, and
the alternate-title case still gets its second chance.

The two beets methods this depends on are patched defensively: if a future
beets renames or reshapes them, the plugin logs and stays out of the way
rather than breaking every import.
"""

from beets import config
from beets.plugins import BeetsPlugin

try:
    from beets.autotag.match import track_distance
    from beetsplug.musicbrainz import MusicBrainzPlugin

    _ORIGINAL_FILTERS = MusicBrainzPlugin.get_search_query_with_filters
    _ORIGINAL_CANDIDATES = MusicBrainzPlugin.item_candidates
except (ImportError, AttributeError):  # pragma: no cover - beets internals moved
    track_distance = None
    MusicBrainzPlugin = None
    _ORIGINAL_FILTERS = None
    _ORIGINAL_CANDIDATES = None


def _strong_threshold():
    """The distance at or below which beets imports without asking."""
    return config["match"]["strong_rec_thresh"].as_number()


def _filters_without_alias(self, query_type, *args, **kwargs):
    """Drop the alias term from track searches unless the fallback asked for it."""
    query, filters = _ORIGINAL_FILTERS(self, query_type, *args, **kwargs)
    if query_type == "track" and not getattr(self, "_mbtwopass_alias", False):
        filters = {k: v for k, v in filters.items() if k != "alias"}
    return query, filters


def _has_strong_match(item, candidates):
    threshold = _strong_threshold()
    for candidate in candidates:
        if track_distance(item, candidate, incl_artist=True).distance <= threshold:
            return True
    return False


def _item_candidates(self, item, artist, title):
    """Search without the alias term, then with it if nothing was close enough."""
    self._mbtwopass_alias = False
    candidates = list(_ORIGINAL_CANDIDATES(self, item, artist, title))

    if candidates and _has_strong_match(item, candidates):
        return candidates

    self._mbtwopass_alias = True
    try:
        fallback = list(_ORIGINAL_CANDIDATES(self, item, artist, title))
    finally:
        self._mbtwopass_alias = False

    seen = {c.track_id for c in candidates}
    extra = [c for c in fallback if c.track_id not in seen]
    if extra:
        self._log.debug(
            "alias fallback added {} candidate(s) for {} - {}",
            len(extra),
            artist,
            title,
        )
    return candidates + extra


class MBTwoPassPlugin(BeetsPlugin):
    def __init__(self):
        super().__init__()

        if MusicBrainzPlugin is None or track_distance is None:
            self._log.warning(
                "beets internals moved, leaving MusicBrainz track search alone"
            )
            return

        MusicBrainzPlugin.get_search_query_with_filters = _filters_without_alias
        MusicBrainzPlugin.item_candidates = _item_candidates
