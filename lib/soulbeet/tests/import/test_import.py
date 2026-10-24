import pytest

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
    assert run.report == {"tasks": [], "pick_missing": False, "refused": None}
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


@pytest.mark.parametrize(
    "source, title",
    [("FakeSrc", "Totally Different 0"), ("OtherSrc", "Other Source Song 1")],
)
def test_pick_keeps_only_the_named_source(beets_env, source, title):
    beets_env.configure(plugins=("fakesrc", "othersrc"))
    track = one_track(beets_env)

    run = beets_env.driver(
        "pick", source, "fake-t-w0", sources=[track], singleton=True, env=WEAK
    )

    assert run.code == 0, run
    assert run.report["pick_missing"] is False
    [(got_title, *_)] = beets_env.library()
    assert got_title == title


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
    assert run.report == {"tasks": [], "pick_missing": False, "refused": None}
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


def test_the_report_is_written_when_beets_crashes(beets_env):
    beets_env.album(beets_env.src)

    run = beets_env.driver(
        "auto", env={"FAKESRC_MODE": "strong", "FAKESRC_CRASH": "1"}
    )

    assert run.code == 1, run
    assert "simulated crash in import stage" in run.stderr
    [task] = run.report["tasks"]
    assert task["candidates"][0]["id"] == "fake-a-1"


def test_older_beets_is_refused(beets_env, tmp_path):
    old = tmp_path / "oldbeets" / "beets"
    (old / "ui").mkdir(parents=True)
    (old / "__init__.py").write_text('__version__ = "1.6.0"\n')
    (old / "ui" / "__init__.py").write_text("")
    # beets 1.6 had the commands in one module.
    (old / "ui" / "commands.py").write_text("class TerminalImportSession: pass\n")
    one_track(beets_env)

    run = beets_env.driver("auto", env={"PYTHONPATH": str(old.parent)})

    assert run.code == 1, run
    assert run.stderr == (
        "error: soulbeet needs beets 2.11 or newer to import (found 1.6.0)\n"
    )
    assert run.report is None
    assert beets_env.files_under(beets_env.src) == ["one.mp3"]
