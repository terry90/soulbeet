# Runs one beets import: beets' own CLI (beets.ui.main) with the import
# session swapped for one that records or steers the choice step.
#
#   python -P -c <this file> REPORT CHOICE -- <beet args>
#
# CHOICE is one of
#   auto            decide as `beet import -q` does, recording each task's candidates
#   asis            import with the files' own tags (-A); refused when one lacks them
#   pick SOURCE ID  apply what SOURCE returns for ID (-S ID), whatever its distance
#
# REPORT gets {"tasks": [...], "pick_missing": bool, "refused": str or null,
# "duplicates": [...], "items": [...]}, also when beets fails. Tasks are
# recorded in auto mode only. "duplicates" holds each duplicate decision beets
# logs as replace, keep or skip, with the library files it is about. "items"
# holds each file beets imported, as it stored it. stdout carries nothing but
# beets' own output.
import json
import os
import sys

import beets


def import_hook():
    """Whether this beets has what import.py hooks into: an import session
    built from a module global it can replace (beets 2.6), and an id on every
    match (2.8). probe.py and import.py each hold this function, and a test
    keeps the two equal."""
    from beets.autotag.hooks import AlbumInfo, TrackInfo
    from beets.ui.commands import import_
    from beets.ui.commands.import_.session import TerminalImportSession

    return (
        import_.TerminalImportSession is TerminalImportSession
        and "TerminalImportSession" in import_.import_files.__code__.co_names
        # Looked up in the package itself, where import.py swaps it.
        and import_.import_files.__globals__ is vars(import_)
        and all(
            isinstance(getattr(info, "id", None), property)
            for info in (AlbumInfo, TrackInfo)
        )
    )


try:
    hooked = import_hook()
except (ImportError, AttributeError):
    hooked = False
if not hooked:
    print(
        "error: soulbeet cannot hook into this beets version's import"
        f" (found {beets.__version__}); it supports beets 2.8 to 2.11",
        file=sys.stderr,
    )
    sys.exit(1)

import beets.ui
from beets import config
from beets.autotag.hooks import AlbumMatch
from beets.autotag.match import Recommendation
from beets.importer import Action
from beets.importer.tasks import ImportTask, ImportTaskFactory, SingletonImportTask
from beets.plugins import BeetsPlugin
from beets.ui.commands import import_ as import_cmd

USAGE = "usage: import.py REPORT (auto | asis | pick SOURCE ID) -- BEET_ARGS"

report = {
    "tasks": [],
    "pick_missing": False,
    "refused": None,
    "duplicates": [],
    "items": [],
}

# The run log statuses of a duplicate decision the report names, as it names
# them.
DUPLICATE_ACTIONS = {
    "duplicate-replace": "replaced",
    "duplicate-keep": "kept",
    "duplicate-skip": "skipped",
}


def path_text(path):
    """A path as report text. The report is read as strict UTF-8, so bytes
    that do not decode become U+FFFD."""
    return os.fsencode(path).decode("utf-8", "replace")


def remember_duplicates(cls, items_of):
    """Keeps, on each task, the library files its duplicate check found.
    beets checks before it decides (importer/stages.py _resolve_duplicates)
    and a skipped task cannot be asked again: chosen_info() refuses a
    task whose choice is SKIP (importer/tasks.py:233-244, 689-699)."""
    find = cls.find_duplicates

    def find_duplicates(self, lib):
        found = find(self, lib)
        self.soulbeet_existing = [path_text(i.path) for i in items_of(found)]
        return found

    cls.find_duplicates = find_duplicates


remember_duplicates(ImportTask, lambda albums: [i for a in albums for i in a.items()])
remember_duplicates(SingletonImportTask, lambda items: items)


class Recorder(BeetsPlugin):
    """Records each item beets imported once its file is in place:
    import_task_files is sent after the move and the store
    (importer/tasks.py:468-514), for non-skipped tasks only (stages.py:276-307)."""

    def __init__(self):
        super().__init__("soulbeet")
        self.register_listener("import_task_files", self.imported)

    def imported(self, task):
        for item in task.imported_items():
            report["items"].append({
                "path": path_text(item.path),
                "recording_mbid": item.mb_trackid or None,
                "artist": item.artist,
                "title": item.title,
                "album": item.album,
            })


Recorder()


def penalty_name(key):
    # The penalty names beets prints with a match (Distance.generic_penalty_keys).
    return key.replace("album_", "").replace("track_", "").replace("_", " ")


def describe(match):
    info, dist = match.info, match.distance
    out = {
        "data_source": info.data_source,
        # Some sources (deezer, tidal) give numeric ids.
        "id": str(info.id),
        "artist": info.artist or "",
        "year": info.get("year") or None,
        "country": info.get("country") or None,
        "per_mille": round((1 - float(dist)) * 1000),
        # Distance.keys() is ordered by weighted penalty, largest first.
        "differs": list(dict.fromkeys(penalty_name(k) for k in dist.keys())),
    }
    if isinstance(match, AlbumMatch):
        out.update(
            album=info.album or "",
            disambig=info.get("albumdisambig") or None,
            track_count=len(info.tracks),
            matched=len(match.mapping),
            extra_items=len(match.extra_items),
        )
    else:
        out.update(
            title=info.title or "",
            album=info.album or None,
            disambig=info.get("trackdisambig") or None,
            length=round(info.length) if info.length else None,
        )
    return out


Session = import_cmd.TerminalImportSession


class Reporting(Session):
    """Reports each duplicate decision as beets logs it, with the library
    files its duplicate check found."""

    deciding = None

    def log_choice(self, task, duplicate=False):
        if not duplicate:
            return super().log_choice(task, duplicate)
        self.deciding = task
        try:
            super().log_choice(task, duplicate)
        finally:
            self.deciding = None

    def tag_log(self, status, paths):
        if self.deciding is not None and status in DUPLICATE_ACTIONS:
            report["duplicates"].append(
                {
                    "action": DUPLICATE_ACTIONS[status],
                    "existing": self.deciding.soulbeet_existing,
                }
            )
        super().tag_log(status, paths)


class AutoSession(Reporting):
    def _record(self, task):
        report["tasks"].append(
            {
                "paths": [path_text(p) for p in task.paths],
                "album": task.is_album,
                # A candidate without an id or a source cannot be picked.
                "candidates": [
                    describe(m)
                    for m in task.candidates
                    if m.info.id is not None and m.info.data_source
                ],
            }
        )

    def choose_match(self, task):
        choice = super().choose_match(task)
        self._record(task)
        return choice

    def choose_item(self, task):
        choice = super().choose_item(task)
        self._record(task)
        return choice


class PickSession(Reporting):
    """Makes the picked candidate the only, strong one, so beets' own quiet
    path applies it with its usual output, events and duplicate handling."""

    source = None
    wanted = None

    def _pin(self, task):
        found = [m for m in task.candidates if m.info.data_source == self.source]
        # Not matched by id: a merged MusicBrainz id comes back as its new id.
        if len(found) != 1:
            report["pick_missing"] = True
            print(
                f"soulbeet: {self.source} has no single match for {self.wanted}"
                f" ({len(found)} found)",
                file=sys.stderr,
            )
            return False
        task.candidates = found
        task.rec = Recommendation.strong
        return True

    def choose_match(self, task):
        return super().choose_match(task) if self._pin(task) else Action.SKIP

    def choose_item(self, task):
        return super().choose_item(task) if self._pin(task) else Action.SKIP


def source_items(session, toppath):
    """The items beets imports from one source, read by beets' own task
    factory, so archives, flat, ignore and extension fixes apply as in the
    import itself."""
    factory = ImportTaskFactory(toppath, session)
    archive = None
    if factory.is_archive:
        archive = factory.unarchive()
        if archive is None:
            # beets imports nothing from an archive it cannot extract.
            return
    try:
        for _dirs, paths in factory.paths():
            for path in paths:
                item = factory.read_item(path)
                if item is not None:
                    yield item
    finally:
        if archive is not None:
            archive.cleanup()


def missing_tags(item, singleton):
    if singleton:
        tags = {"artist": item.artist, "title": item.title}
    else:
        tags = {
            "album": item.album,
            "artist": item.albumartist or item.artist,
            "title": item.title,
        }
    return [name for name, value in tags.items() if not value.strip()]


def untagged_refusal(session):
    """Why an as-is import of the session's sources is refused, or None.

    beets files untagged music under empty names, and with duplicate_action
    remove a second untagged import deletes the first one."""
    singleton = session.config["singletons"].get(bool)
    lacking = [
        path_text(os.path.basename(item.path))
        for toppath in session.paths
        for item in source_items(session, toppath)
        if missing_tags(item, singleton)
    ]
    if not lacking:
        return None
    need = "artist and title" if singleton else "album, artist and title"
    files = "1 file lacks" if len(lacking) == 1 else f"{len(lacking)} files lack"
    shown = ", ".join(lacking[:3])
    if len(lacking) > 3:
        shown += f" and {len(lacking) - 3} more"
    return f"Import as-is needs {need} tags, and {files} them: {shown}"


class AsIsSession(Reporting):
    def run(self):
        # The task factory reads the session config, which run() sets only
        # after it opens the run log.
        self.set_config(config["import"])
        refused = untagged_refusal(self)
        if refused is not None:
            report["refused"] = refused
            print(f"soulbeet: {refused}", file=sys.stderr)
            return
        super().run()


def parse(argv):
    """(report path, session class, beet args) from the command line."""
    if "--" not in argv[1:]:
        sys.exit(USAGE)
    sep = argv.index("--", 1)
    report_path, choice, beet_args = argv[0], argv[1:sep], argv[sep + 1 :]
    if choice == ["auto"]:
        return report_path, AutoSession, beet_args
    if choice == ["asis"]:
        return report_path, AsIsSession, [*beet_args, "-A"]
    if len(choice) == 3 and choice[0] == "pick":
        PickSession.source, PickSession.wanted = choice[1], choice[2]
        return report_path, PickSession, [*beet_args, "-S", choice[2]]
    sys.exit(USAGE)


report_path, session, beet_args = parse(sys.argv[1:])
import_cmd.TerminalImportSession = session
try:
    beets.ui.main(beet_args)
finally:
    # A lone surrogate in a plugin's text would make the report invalid UTF-8,
    # so it is written as "?".
    with open(report_path, "w", encoding="utf-8", errors="replace") as f:
        json.dump(report, f, ensure_ascii=False)
