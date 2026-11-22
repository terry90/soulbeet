import os
import sqlite3
import subprocess
import sys
from pathlib import Path

import pytest

PRUNE_SRC = (Path(__file__).resolve().parents[2] / "src" / "beets" / "prune.py").read_text()

STRONG = {"FAKESRC_MODE": "strong"}


def prune(env):
    """Runs prune.py on the library as soulbeet runs it: the finished process and the
    summary lines, without their marker."""
    proc = subprocess.run(
        [
            sys.executable, "-P", "-c", PRUNE_SRC,
            "-c", str(env.config),
            "-l", str(env.music / ".beets_library.db"),
            "-d", str(env.music),
            "soulbeet-prune",
        ],
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
        env=env._env(),
        cwd=env.root,
        timeout=120,
    )
    summary = [line[6:] for line in proc.stdout.splitlines() if line.startswith("@@sb@@")]
    return proc, summary


def imported_album(env):
    """The two files of an album beets imported, in track order."""
    env.album(env.src)
    run = env.driver("auto", env=STRONG)
    assert run.code == 0, run
    return [env.music / row[4] for row in sorted(env.library(), key=lambda r: r[0])]


def stored_paths(env):
    """Each item's id by its path as the library database holds it."""
    with sqlite3.connect(env.music / ".beets_library.db") as db:
        return {path: item_id for item_id, path in db.execute("SELECT id, path FROM items")}


def test_an_item_whose_file_is_gone_is_dropped_and_its_file_left_alone(beets_env):
    first, second = imported_album(beets_env)
    first.unlink()

    proc, summary = prune(beets_env)
    assert (proc.returncode, summary) == (0, ["pruned 1 1"]), proc
    assert [beets_env.music / row[4] for row in beets_env.library()] == [second]
    assert second.exists()
    assert f"soulbeet: dropped {first}" in proc.stdout.splitlines()

    # A second run finds nothing.
    proc, summary = prune(beets_env)
    assert (proc.returncode, summary) == (0, ["pruned 0 1"]), proc


def test_a_library_none_of_whose_files_is_there_is_left_whole(beets_env, tmp_path):
    first, _ = imported_album(beets_env)
    # As when the share holding the library folder is not mounted.
    away = tmp_path / "away"
    first.parent.parent.rename(away)

    proc, summary = prune(beets_env)
    assert (proc.returncode, summary) == (0, ["unproven 2"]), proc
    assert len(beets_env.library()) == 2

    away.rename(first.parent.parent)
    proc, summary = prune(beets_env)
    assert (proc.returncode, summary) == (0, ["pruned 0 2"]), proc


@pytest.mark.skipif(os.geteuid() == 0, reason="root reads any folder")
def test_an_item_in_a_folder_that_cannot_be_read_is_kept(beets_env):
    first, _ = imported_album(beets_env)
    first.unlink()
    folder = first.parent
    folder.chmod(0)
    try:
        proc, summary = prune(beets_env)
    finally:
        folder.chmod(0o755)
    assert (proc.returncode, summary) == (0, ["pruned 0 2"]), proc
    assert len(beets_env.library()) == 2


@pytest.mark.parametrize("gone", ["absolute row", "row as beets stores it"])
def test_rows_stored_absolute_and_as_beets_stores_them_are_both_read(beets_env, gone):
    first, second = imported_album(beets_env)
    music = os.fsencode(beets_env.music)
    ids = {os.path.join(music, path): item_id for path, item_id in stored_paths(beets_env).items()}
    # beets 2.10+ stores a path inside the library folder relative to it; an
    # earlier version stored it absolute, as the first row now is.
    with sqlite3.connect(beets_env.music / ".beets_library.db") as db:
        db.execute(
            "UPDATE items SET path = ? WHERE id = ?",
            (os.fsencode(first), ids[os.fsencode(first)]),
        )
    assert os.fsencode(first) in stored_paths(beets_env)
    assert os.fsencode(second) in ids

    dropped, kept = (first, second) if gone == "absolute row" else (second, first)
    dropped.unlink()
    proc, summary = prune(beets_env)
    assert (proc.returncode, summary) == (0, ["pruned 1 1"]), proc
    assert [beets_env.music / row[4] for row in beets_env.library()] == [kept]
