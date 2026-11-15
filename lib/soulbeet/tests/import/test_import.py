import ast
import os
import zipfile
from pathlib import Path

import pytest
import yaml

SRC = Path(__file__).resolve().parents[2] / "src" / "beets"
DRIVER_SRC = (SRC / "import.py").read_text()
PROBE_SRC = (SRC / "probe.py").read_text()
FAKE_BEETS = SRC.parents[1] / "tests" / "fakebeets"

WEAK = {"FAKESRC_MODE": "weak"}


def normalized(run, env):
    root = str(env.root)
    return (
        run.stdout.replace(root, "ROOT"),
        [line.replace(root, "ROOT") for line in run.runlog_lines],
    )


def one_track(env):
    return env.mp3(env.src / "one.mp3", artist="Zzqx Artist", title="Song Number 1")


@pytest.mark.parametrize("singleton", [False, True], ids=["album", "singleton"])
@pytest.mark.parametrize("mode", ["strong", "weak"])
def test_auto_decides_like_plain_beet(beets_env, plain_env, mode, singleton):
    env = {"FAKESRC_MODE": mode}
    for setup in (beets_env, plain_env):
        setup.album(setup.src)
    ours = beets_env.driver("auto", singleton=singleton, env=env)
    theirs = plain_env.plain(singleton=singleton, env=env)

    assert ours.code == theirs.code == 0, (ours, theirs)
    assert normalized(ours, beets_env) == normalized(theirs, plain_env)
    assert beets_env.music_files() == plain_env.music_files()
    assert beets_env.files_under(beets_env.src) == plain_env.files_under(
        plain_env.src
    )
    if mode == "weak":
        assert beets_env.music_files() == []
    else:
        assert len(beets_env.music_files()) == 2


def test_auto_records_a_skipped_album(beets_env):
    beets_env.album(beets_env.src)
    run = beets_env.driver("auto", env=WEAK)

    assert run.runlog_lines == [f"skip {beets_env.src}"], run
    [task] = run.report["tasks"]
    assert task["paths"] == [str(beets_env.src)]
    assert task["album"] is True
    assert [c["id"] for c in task["candidates"]] == [
        "fake-a-w0",
        "fake-a-w1",
        "fake-a-w2",
    ]
    assert task["candidates"][0] == {
        "data_source": "FakeSrc",
        "id": "fake-a-w0",
        "artist": "Some Other Artist",
        "album": "Other Album 0",
        "year": None,
        "country": None,
        "disambig": None,
        "per_mille": 370,
        # Largest weighted penalty first.
        "differs": ["tracks", "artist", "album"],
        "track_count": 2,
        "matched": 2,
        "extra_items": 0,
    }
    assert run.report["pick_missing"] is False
    assert run.report["refused"] is None


def test_auto_records_a_skipped_track(beets_env):
    track = one_track(beets_env)
    run = beets_env.driver("auto", singleton=True, env=WEAK)

    assert run.runlog_lines == [f"skip {track}"], run
    [task] = run.report["tasks"]
    assert task["paths"] == [str(track)]
    assert task["album"] is False
    # In beets' order, best first.
    assert [c["id"] for c in task["candidates"]] == [
        "fake-t-w1",
        "fake-t-w0",
        "fake-t-w2",
    ]
    assert task["candidates"][0] == {
        "data_source": "FakeSrc",
        "id": "fake-t-w1",
        "artist": "Some Other Artist",
        "title": "Totally Different 1",
        "album": None,
        "year": None,
        "country": None,
        "disambig": None,
        "per_mille": 215,
        "differs": ["title", "length", "artist"],
        "length": 202,
    }
    assert run.report["pick_missing"] is False
    assert run.report["refused"] is None


def test_auto_records_an_applied_match(beets_env):
    beets_env.album(beets_env.src)
    run = beets_env.driver("auto", env={"FAKESRC_MODE": "strong"})

    assert "Match (100.0%)" in run.stdout, run
    [task] = run.report["tasks"]
    [candidate] = task["candidates"]
    assert candidate["id"] == "fake-a-1"
    assert candidate["per_mille"] == 1000
    assert candidate["differs"] == []
    assert len(beets_env.music_files()) == 2


def test_a_replaced_duplicate_is_reported_with_the_file_it_replaced(beets_env):
    beets_env.album(beets_env.src)
    first = beets_env.driver("auto", env={"FAKESRC_MODE": "strong"})
    assert first.report["duplicates"] == []
    old = sorted(str(beets_env.music / f) for f in beets_env.music_files())

    beets_env.album(beets_env.src)
    second = beets_env.driver("auto", env={"FAKESRC_MODE": "strong"})

    [dup] = second.report["duplicates"]
    assert (dup["action"], sorted(dup["existing"])) == ("replaced", old), second
    assert any(line.startswith("duplicate-replace ") for line in second.runlog_lines)


def test_replaced_singletons_are_reported_with_the_tracks_they_replaced(beets_env):
    beets_env.album(beets_env.src)
    beets_env.driver("auto", singleton=True, env={"FAKESRC_MODE": "strong"})
    old = sorted(str(beets_env.music / f) for f in beets_env.music_files())

    beets_env.album(beets_env.src)
    run = beets_env.driver("auto", singleton=True, env={"FAKESRC_MODE": "strong"})

    assert [d["action"] for d in run.report["duplicates"]] == ["replaced", "replaced"], run
    assert [len(d["existing"]) for d in run.report["duplicates"]] == [1, 1]
    assert sorted(p for d in run.report["duplicates"] for p in d["existing"]) == old


def test_a_path_query_finds_the_one_file_at_that_path(beets_env):
    beets_env.album(beets_env.src)
    beets_env.driver("auto", env={"FAKESRC_MODE": "strong"})
    first = beets_env.music / beets_env.library()[0][4]

    def ids(path):
        rows = beets_env.ls("@@sb@@$id", f"path:{path}")
        return [row for row in rows if row.startswith("@@sb@@")]

    assert len(ids(first)) == 1
    assert len(ids(first.parent)) == 2


REPORTED_STATUS = {
    "duplicate-replace": "replaced",
    "duplicate-keep": "kept",
    "duplicate-skip": "skipped",
}


def import_twice(beets_env, duplicate_action):
    """The second import of one album under `duplicate_action`."""
    cfg = yaml.safe_load(beets_env.config.read_text())
    cfg["import"]["duplicate_action"] = duplicate_action
    beets_env.config.write_text(yaml.safe_dump(cfg))
    beets_env.album(beets_env.src)
    beets_env.driver("auto", env={"FAKESRC_MODE": "strong"})
    beets_env.album(beets_env.src)
    return beets_env.driver("auto", env={"FAKESRC_MODE": "strong"})


@pytest.mark.parametrize(
    "action,statuses,reported",
    [("skip", ["duplicate-skip"], ["skipped"]), ("keep", ["duplicate-keep"], ["kept"])],
)
def test_a_duplicate_decision_is_reported_as_beets_logged_it(
    beets_env, action, statuses, reported
):
    run = import_twice(beets_env, action)

    assert [line.split(" ", 1)[0] for line in run.runlog_lines] == statuses, run
    assert [d["action"] for d in run.report["duplicates"]] == reported, run


# How many times merge logs duplicate-keep depends on beets: 2.11 logs it twice
# and ends in a skip (fixture dup_merge/runlog.txt), 2.8 finds no duplicate of
# the merged task and imports it after one.
def test_a_merge_is_reported_as_the_keep_beets_logs(beets_env):
    run = import_twice(beets_env, "merge")

    statuses = [line.split(" ", 1)[0] for line in run.runlog_lines]
    assert statuses, run
    assert statuses[0] == "duplicate-keep", run
    actions = [d["action"] for d in run.report["duplicates"]]
    assert actions == [REPORTED_STATUS[s] for s in statuses if s in REPORTED_STATUS]
    assert set(actions) == {"kept"}, run


def test_a_skipped_duplicate_names_the_library_files_it_kept(beets_env):
    run = import_twice(beets_env, "skip")

    [dup] = run.report["duplicates"]
    assert dup["action"] == "skipped", run
    assert sorted(dup["existing"]) == sorted(
        str(beets_env.music / f) for f in beets_env.music_files()
    )
    assert run.report["items"] == []


def test_a_wanted_removal_is_reported_and_the_earlier_copy_kept(beets_env):
    beets_env.album(beets_env.src)
    beets_env.driver("auto", env={"FAKESRC_MODE": "strong"})
    old = sorted(str(beets_env.music / f) for f in beets_env.music_files())

    beets_env.album(beets_env.src)
    run = beets_env.driver("auto", duplicates="report-removals", env={"FAKESRC_MODE": "strong"})

    [wanted] = run.report["duplicates"]
    assert wanted["action"] == "removal_wanted", run
    assert sorted(i["path"] for i in wanted["existing"]) == old
    assert all(i["artist"] and i["title"] and i["album"] for i in wanted["existing"])
    assert len(wanted["imported"]) == 2
    assert sorted(i["path"] for i in wanted["imported"]) == sorted(
        i["path"] for i in run.report["items"]
    )
    assert all(Path(p).exists() for p in old)
    assert len(beets_env.music_files()) == 4
    assert [line.split(" ", 1)[0] for line in run.runlog_lines] == ["duplicate-keep"]


def test_no_ids_are_reported_for_a_task_whose_files_did_not_reach_the_library(beets_env):
    beets_env.album(beets_env.src)
    beets_env.driver("auto", env={"FAKESRC_MODE": "strong"})
    old = sorted(str(beets_env.music / f) for f in beets_env.music_files())
    # The second album's folder (%aunique{}) would be made in the artist folder.
    artist = Path(old[0]).parent.parent
    beets_env.album(beets_env.src)
    artist.chmod(0o555)
    try:
        run = beets_env.driver("auto", duplicates="report-removals", env={"FAKESRC_MODE": "strong"})
    finally:
        artist.chmod(0o755)
    assert run.code != 0, run
    [wanted] = run.report["duplicates"]
    assert (wanted["action"], wanted["imported"]) == ("removal_wanted", []), run
    assert all(Path(p).exists() for p in old)


def test_library_items_outside_the_library_folder_are_not_reported_for_removal(beets_env):
    # Imported in place, so the earlier items lie outside the library folder, where beets'
    # remove would only drop them from its database (importer/tasks.py:281 in 2.11, :275 in 2.8).
    cfg = yaml.safe_load(beets_env.config.read_text())
    cfg["import"].update(copy=False, move=False)
    beets_env.config.write_text(yaml.safe_dump(cfg))
    beets_env.album(beets_env.src)
    beets_env.driver("auto", env={"FAKESRC_MODE": "strong"})
    cfg["import"]["move"] = True
    beets_env.config.write_text(yaml.safe_dump(cfg))
    second = beets_env.root / "src2"
    beets_env.album(second)
    run = beets_env.driver("auto", sources=[second], duplicates="report-removals", env={"FAKESRC_MODE": "strong"})
    assert run.code == 0, run
    assert run.report["duplicates"] == [], run
    assert [line.split(" ", 1)[0] for line in run.runlog_lines] == ["duplicate-keep"]


@pytest.mark.parametrize("action,reported", [("skip", ["skipped"]), ("keep", ["kept"])])
def test_reporting_removals_leaves_other_duplicate_actions_alone(beets_env, action, reported):
    cfg = yaml.safe_load(beets_env.config.read_text())
    cfg["import"]["duplicate_action"] = action
    beets_env.config.write_text(yaml.safe_dump(cfg))
    beets_env.album(beets_env.src)
    beets_env.driver("auto", env={"FAKESRC_MODE": "strong"})
    beets_env.album(beets_env.src)
    run = beets_env.driver("auto", duplicates="report-removals", env={"FAKESRC_MODE": "strong"})
    assert [d["action"] for d in run.report["duplicates"]] == reported, run


def test_moving_the_new_items_after_the_earlier_ones_are_gone_gives_them_their_paths(beets_env):
    beets_env.album(beets_env.src)
    beets_env.driver("auto", env={"FAKESRC_MODE": "strong"})
    beets_env.album(beets_env.src)
    run = beets_env.driver("auto", duplicates="report-removals", env={"FAKESRC_MODE": "strong"})
    [wanted] = run.report["duplicates"]
    assert sorted(i["path"] for i in wanted["imported"]) != sorted(
        i["path"] for i in wanted["existing"]
    ), run
    for item in wanted["existing"]:
        os.remove(item["path"])
        beets_env.beet("remove", "-f", f"path:{item['path']}")
    ids = [i["id"] for i in wanted["imported"]]
    query = [part for i in ids for part in (f"id:{i}", ",")][:-1]
    beets_env.beet("move", *query)
    assert sorted(str(beets_env.music / f) for f in beets_env.music_files()) == sorted(
        i["path"] for i in wanted["existing"]
    )


# Sets duplicate_action back to remove once the run has begun, as a user's own
# plugin in /data/beets-plugins could.
RESET_TO_REMOVE = """
from beets import config
from beets.plugins import BeetsPlugin


class ResetDuplicates(BeetsPlugin):
    def __init__(self):
        super().__init__()
        self.register_listener("import_begin", self.reset)

    def reset(self, session):
        config["import"]["duplicate_action"] = "remove"
"""


@pytest.mark.parametrize("singleton", [False, True], ids=["album", "singleton"])
def test_beets_stops_before_deleting_in_a_run_that_reports_removals(
    beets_env, tmp_path, singleton
):
    beets_env.album(beets_env.src)
    beets_env.driver("auto", singleton=singleton, env={"FAKESRC_MODE": "strong"})
    old = sorted(str(beets_env.music / f) for f in beets_env.music_files())
    plugins = tmp_path / "user-plugins"
    plugins.mkdir()
    (plugins / "resetduplicates.py").write_text(RESET_TO_REMOVE)
    cfg = yaml.safe_load(beets_env.config.read_text())
    cfg["plugins"] += " resetduplicates"
    cfg["pluginpath"].append(str(plugins))
    beets_env.config.write_text(yaml.safe_dump(cfg))

    beets_env.album(beets_env.src)
    run = beets_env.driver(
        "auto",
        singleton=singleton,
        duplicates="report-removals",
        env={"FAKESRC_MODE": "strong"},
    )

    assert run.code != 0, run
    assert (
        "soulbeet: beets was about to delete library files in a run that reports removals"
        in run.stderr
    ), run
    assert len(old) == 2
    assert all(Path(p).exists() for p in old)


@pytest.mark.parametrize("singleton", [False, True], ids=["album", "singleton"])
def test_imported_items_are_reported_as_beets_stored_them(beets_env, singleton):
    beets_env.album(beets_env.src)
    run = beets_env.driver("auto", singleton=singleton, env={"FAKESRC_MODE": "strong"})

    assert run.code == 0, run
    reported = sorted(
        (i["path"], i["recording_mbid"], i["artist"], i["title"], i["album"])
        for i in run.report["items"]
    )
    stored = sorted(
        (str(beets_env.music / path), trackid or None, artist, title, album)
        for title, artist, album, trackid, path in beets_env.library()
    )
    assert reported == stored
    assert len(reported) == 2


def test_an_as_is_import_reports_its_items_without_a_recording(beets_env):
    one_track(beets_env)
    run = beets_env.driver("asis", singleton=True)

    assert run.code == 0, run
    assert run.report["items"] == [{
        "path": str(beets_env.music / "Zzqx Artist/Song Number 1.mp3"),
        "recording_mbid": None,
        "artist": "Zzqx Artist",
        "title": "Song Number 1",
        "album": "",
    }]


def test_pick_applies_a_weak_track_and_tags_it(beets_env):
    track = one_track(beets_env)
    skipped = beets_env.driver("auto", singleton=True, env=WEAK)
    assert skipped.runlog_lines == [f"skip {track}"]

    run = beets_env.driver(
        "pick", "FakeSrc", "fake-t-w1", sources=[track], singleton=True, env=WEAK
    )

    assert run.code == 0, run
    assert run.runlog_lines == []
    assert "Match (" in run.stdout
    assert run.report == {
        "tasks": [],
        "pick_missing": False,
        "refused": None,
        "duplicates": [],
        "items": [
            {
                "path": str(beets_env.music / "Some Other Artist/Totally Different 1.mp3"),
                "recording_mbid": "fake-t-w1",
                "artist": "Some Other Artist",
                "title": "Totally Different 1",
                "album": "",
            }
        ],
    }
    assert not track.exists()
    assert beets_env.music_files() == ["Some Other Artist/Totally Different 1.mp3"]
    assert beets_env.library() == [
        (
            "Totally Different 1",
            "Some Other Artist",
            "",
            "fake-t-w1",
            "Some Other Artist/Totally Different 1.mp3",
        )
    ]
    tags = beets_env.tags(
        beets_env.music / "Some Other Artist/Totally Different 1.mp3"
    )
    assert tags["title"] == "Totally Different 1"
    assert tags["artist"] == "Some Other Artist"


def test_pick_applies_a_weak_album_and_tags_it(beets_env):
    beets_env.album(beets_env.src)
    skipped = beets_env.driver("auto", env=WEAK)
    assert skipped.runlog_lines == [f"skip {beets_env.src}"]

    run = beets_env.driver("pick", "FakeSrc", "fake-a-w2", env=WEAK)

    assert run.code == 0, run
    assert run.runlog_lines == []
    assert run.report["pick_missing"] is False
    assert beets_env.music_files() == [
        "Some Other Artist/Other Album 2/01 Totally Different 1.mp3",
        "Some Other Artist/Other Album 2/02 Totally Different 2.mp3",
    ]
    tags = beets_env.tags(
        beets_env.music / "Some Other Artist/Other Album 2/02 Totally Different 2.mp3"
    )
    assert tags["album"] == "Other Album 2"
    assert tags["title"] == "Totally Different 2"
    assert beets_env.files_under(beets_env.src) == []


def test_pick_of_a_shorter_release_leaves_the_extra_file(beets_env):
    beets_env.album(beets_env.src, n=3)

    run = beets_env.driver("pick", "FakeSrc", "fake-a-w2", env=WEAK)

    assert run.code == 0, run
    # The classifier counts the files left behind from these two lines.
    lines = run.stdout.splitlines()
    assert f"{beets_env.src} (3 items)" in lines
    assert "Unmatched tracks (1):" in lines
    assert len(beets_env.music_files()) == 2
    assert beets_env.files_under(beets_env.src) == ["03 - Song Number 3.mp3"]


@pytest.mark.parametrize(
    "singleton, wanted",
    [(True, "fake-t-w9"), (False, "fake-a-w9")],
    ids=["singleton", "album"],
)
def test_pick_of_an_unknown_id_moves_nothing(beets_env, singleton, wanted):
    if singleton:
        source = one_track(beets_env)
    else:
        beets_env.album(beets_env.src)
        source = beets_env.src
    before = beets_env.files_under(beets_env.src)

    run = beets_env.driver(
        "pick", "FakeSrc", wanted, sources=[source], singleton=singleton, env=WEAK
    )

    assert run.code == 0, run
    assert run.report["pick_missing"] is True
    expected = f"soulbeet: FakeSrc has no single match for {wanted} (0 found)"
    assert expected in run.stderr
    assert run.runlog_lines == [f"skip {source}"]
    assert beets_env.files_under(beets_env.src) == before
    assert beets_env.music_files() == []


# What each source's lookup of the id names, read from the library row.
PICKED = {
    "singleton": ("fake-t-w0", 0, {"FakeSrc": "Totally Different 0",
                                   "OtherSrc": "Other Source Song 1"}),
    "album": ("fake-a-w0", 2, {"FakeSrc": "Other Album 0",
                               "OtherSrc": "Other Source Album"}),
}


@pytest.mark.parametrize("source", ["FakeSrc", "OtherSrc"])
@pytest.mark.parametrize("mode", ["singleton", "album"])
def test_pick_keeps_only_the_named_source(beets_env, mode, source):
    wanted, column, names = PICKED[mode]
    beets_env.configure(plugins=("fakesrc", "othersrc"))
    singleton = mode == "singleton"
    if singleton:
        one_track(beets_env)
    else:
        beets_env.album(beets_env.src)

    run = beets_env.driver("pick", source, wanted, singleton=singleton, env=WEAK)

    assert run.code == 0, run
    assert run.report["pick_missing"] is False
    rows = beets_env.library()
    assert len(rows) == (1 if singleton else 2)
    assert {row[column] for row in rows} == {names[source]}


@pytest.mark.parametrize("singleton", [False, True], ids=["album", "singleton"])
def test_asis_imports_with_the_files_own_tags(beets_env, singleton):
    if singleton:
        sources = [one_track(beets_env)]
        expected = ["Zzqx Artist/Song Number 1.mp3"]
    else:
        beets_env.album(beets_env.src)
        sources = [beets_env.src]
        expected = [
            "Zzqx Artist/Qzz Album/01 Song Number 1.mp3",
            "Zzqx Artist/Qzz Album/02 Song Number 2.mp3",
        ]
    first = sorted(beets_env.src.iterdir())[0]
    tags_before = beets_env.tags(first)

    # Any lookup would log an error, so a clean run shows none was made.
    run = beets_env.driver(
        "asis", sources=sources, singleton=singleton, env={"FAKESRC_MODE": "error"}
    )

    assert run.code == 0, run
    assert "Error in 'FakeSrc" not in run.stderr
    assert {k: v for k, v in run.report.items() if k != "items"} == {
        "tasks": [],
        "pick_missing": False,
        "refused": None,
        "duplicates": [],
    }
    assert run.runlog_lines == []
    assert run.stdout == ""
    assert beets_env.music_files() == expected
    assert beets_env.tags(beets_env.music / expected[0]) == tags_before
    assert all(trackid == "" for _, _, _, trackid, _ in beets_env.library())


def test_asis_refuses_an_untagged_track(beets_env):
    track = beets_env.mp3(beets_env.src / "one.mp3", title="Only A Title")

    run = beets_env.driver("asis", sources=[track], singleton=True)

    assert run.code == 0, run
    assert run.report["refused"] == (
        "Import as-is needs artist and title tags, and 1 file lacks them: one.mp3"
    )
    assert track.exists()
    assert beets_env.music_files() == []
    assert run.runlog_lines == []


def test_asis_refuses_an_album_when_any_file_lacks_tags(beets_env):
    beets_env.album(beets_env.src)
    beets_env.mp3(beets_env.src / "03 - bare.mp3", artist="Zzqx Artist", title="Bare")
    for i in range(4, 8):
        beets_env.mp3(beets_env.src / f"0{i}.mp3")
    (beets_env.src / "cover.jpg").write_bytes(b"not audio")
    before = beets_env.files_under(beets_env.src)

    run = beets_env.driver("asis")

    assert run.code == 0, run
    assert run.report["refused"] == (
        "Import as-is needs album, artist and title tags, and 5 files lack them:"
        " 03 - bare.mp3, 04.mp3, 05.mp3 and 2 more"
    )
    assert beets_env.files_under(beets_env.src) == before
    assert beets_env.music_files() == []


def test_asis_reads_inside_an_archive(beets_env, tmp_path):
    staged = tmp_path / "staged"
    beets_env.album(staged)
    beets_env.mp3(staged / "03.mp3")
    archive = beets_env.src / "album.zip"
    with zipfile.ZipFile(archive, "w") as z:
        for f in sorted(staged.iterdir()):
            z.write(f, f.name)
    extract_to = tmp_path / "extract"
    extract_to.mkdir()

    run = beets_env.driver(
        "asis", sources=[archive], env={"TMPDIR": str(extract_to)}
    )

    assert run.code == 0, run
    assert run.report["refused"] == (
        "Import as-is needs album, artist and title tags, and 1 file lacks them:"
        " 03.mp3"
    )
    assert beets_env.files_under(beets_env.src) == ["album.zip"]
    assert beets_env.music_files() == []
    # The check's extraction is removed again.
    assert list(extract_to.iterdir()) == []


def test_asis_imports_a_tagged_archive(beets_env, tmp_path):
    staged = tmp_path / "staged"
    beets_env.album(staged)
    archive = beets_env.src / "album.zip"
    with zipfile.ZipFile(archive, "w") as z:
        for f in sorted(staged.iterdir()):
            z.write(f, f.name)

    run = beets_env.driver("asis", sources=[archive], env={"FAKESRC_MODE": "error"})

    assert run.code == 0, run
    assert run.report["refused"] is None
    assert beets_env.music_files() == [
        "Zzqx Artist/Qzz Album/01 Song Number 1.mp3",
        "Zzqx Artist/Qzz Album/02 Song Number 2.mp3",
    ]


# A Latin-1 name, which is not valid UTF-8.
LATIN1_NAME = os.fsdecode(b"caf\xe9.mp3")


def test_auto_reports_a_non_utf8_path_as_text(beets_env):
    track = beets_env.mp3(
        beets_env.src / LATIN1_NAME, artist="Zzqx Artist", title="Song Number 1"
    )

    run = beets_env.driver("auto", singleton=True, env=WEAK)

    assert run.code == 0, run
    [task] = run.report["tasks"]
    assert task["paths"] == [str(track.parent / "caf\ufffd.mp3")]


def test_asis_refusal_names_a_non_utf8_file_as_text(beets_env):
    beets_env.mp3(beets_env.src / LATIN1_NAME, title="Only A Title")

    run = beets_env.driver("asis", singleton=True)

    assert run.code == 0, run
    assert run.report["refused"] == (
        "Import as-is needs artist and title tags, and 1 file lacks them:"
        " caf\ufffd.mp3"
    )


def test_a_lone_surrogate_from_a_source_is_written_as_utf8(beets_env):
    beets_env.configure(plugins=("fakesrc", "othersrc"))
    one_track(beets_env)

    run = beets_env.driver(
        "auto", singleton=True, env={**WEAK, "OTHERSRC_ARTIST": LATIN1_NAME}
    )

    assert run.code == 0, run
    [task] = run.report["tasks"]
    [other] = [c for c in task["candidates"] if c["data_source"] == "OtherSrc"]
    assert other["artist"] == "caf?.mp3"


def test_candidates_are_reported_with_a_text_id_and_a_source(beets_env):
    beets_env.configure(plugins=("fakesrc", "othersrc"))
    one_track(beets_env)

    run = beets_env.driver(
        "auto", singleton=True, env={**WEAK, "OTHERSRC_ODD_IDS": "1"}
    )

    assert run.code == 0, run
    [task] = run.report["tasks"]
    # The numeric id comes back as text. The ones without an id or a data
    # source are left out, so every candidate has both.
    assert all(c["data_source"] and c["id"] for c in task["candidates"])
    others = [c["id"] for c in task["candidates"] if c["data_source"] == "OtherSrc"]
    assert others == ["3135556"]


def test_the_report_is_written_when_beets_crashes(beets_env):
    beets_env.album(beets_env.src)

    run = beets_env.driver(
        "auto", env={"FAKESRC_MODE": "strong", "FAKESRC_CRASH": "1"}
    )

    assert run.code == 1, run
    assert "simulated crash in import stage" in run.stderr
    [task] = run.report["tasks"]
    assert task["candidates"][0]["id"] == "fake-a-1"


def test_the_probe_holds_the_same_hook_check():
    def hook(src):
        [fn] = [
            node
            for node in ast.parse(src).body
            if isinstance(node, ast.FunctionDef) and node.name == "import_hook"
        ]
        return ast.get_source_segment(src, fn)

    assert hook(DRIVER_SRC) == hook(PROBE_SRC)


# Layouts of beets that the hook check must refuse: 1.6 has no swappable
# import session, 2.6 and 2.7 have one but no id on a match, and "moved"
# re-exports an import_files that would not see the swapped session.
@pytest.mark.parametrize(
    "layout, version", [("1.6", "1.6.0"), ("2.6", "2.6.0"), ("moved", "2.99.0")]
)
def test_beets_without_the_hook_is_refused(beets_env, layout, version):
    one_track(beets_env)

    run = beets_env.driver("auto", env={"PYTHONPATH": str(FAKE_BEETS / layout)})

    assert run.code == 1, run
    assert run.stderr == (
        "error: soulbeet cannot hook into this beets version's import"
        f" (found {version}); it supports beets 2.8 to 2.11\n"
    )
    assert run.report is None
    assert beets_env.files_under(beets_env.src) == ["one.mp3"]
