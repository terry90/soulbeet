"""The helper as Soulbeet runs it, on fake yt-dlp and ytmusicapi packages that answer with what
YouTube Music answered on 2026-11-25. The Rust tests read the same answers as the helper prints
them, from src/ytdlp/fixtures: each fixture must be what the helper prints."""

import json
from pathlib import Path

import pytest

from conftest import fixture_lines


def test_version_names_each_piece(helper):
    (line,) = helper({"command": "version"})
    found = line["version"]
    assert found["yt_dlp"] == "2026.11.23"
    assert found["ejs"] == "0.8.0"
    assert found["ytmusicapi"] == "1.12.4"
    assert found["deno"] == "2.9.8"
    assert found["ffmpeg"] == "8.0.1"
    assert found["python"]


def test_version_of_an_image_without_the_tools_is_null_not_an_error(helper, tmp_path):
    empty = tmp_path / "site-packages"
    empty.mkdir()
    (line,) = helper(
        {"command": "version"},
        site_packages=empty,
        deno=str(tmp_path / "no-deno"),
        ffmpeg=None,
    )
    found = line["version"]
    for piece in ("yt_dlp", "ejs", "ytmusicapi", "deno", "ffmpeg"):
        assert found[piece] is None, piece


@pytest.mark.parametrize(
    "fixture, request_",
    [
        ("slint-albums", {"command": "search_albums", "query": "Slint Spiderland", "limit": 5}),
        ("slint-spiderland", {"command": "album", "browse_id": "MPREb_ur8iGYISD11"}),
        ("slint-songs", {"command": "search_songs", "query": "Slint Breadcrumb Trail", "limit": 20}),
        (
            "monkeys-songs",
            {"command": "search_songs", "query": "Kevin MacLeod Monkeys Spinning Monkeys", "limit": 20},
        ),
        ("luther-songs", {"command": "search_songs", "query": "Kendrick Lamar luther", "limit": 20}),
    ],
)
def test_searches_print_the_fixtures_the_rust_tests_read(helper, fixture, request_):
    assert helper(request_) == fixture_lines(fixture)


def test_a_search_keeps_only_what_soulbeet_reads(helper):
    (line,) = helper({"command": "search_songs", "query": "Slint Breadcrumb Trail", "limit": 3})
    songs = line["songs"]
    assert len(songs) == 3
    assert songs[0] == {
        "video_id": "gBfoQjJ7guk",
        "title": "Breadcrumb Trail",
        "artists": ["Slint"],
        "album": {"name": "Spiderland", "browse_id": "MPREb_ur8iGYISD11"},
        "duration_s": 356,
        "available": True,
        "video_type": "MUSIC_VIDEO_TYPE_ATV",
        "explicit": False,
    }


def test_a_search_youtube_music_finds_nothing_for_is_empty(helper):
    assert helper({"command": "search_albums", "query": "Nobody Nothing", "limit": 5}) == [
        {"albums": []}
    ]


def test_a_rate_limited_search_says_so(helper):
    (line,) = helper(
        {"command": "search_songs", "query": "fail: Server returned HTTP 429: Too Many Requests.", "limit": 5}
    )
    assert line["error"]["kind"] == "rate_limited"


def test_a_browse_id_that_is_not_one_is_refused(helper):
    (line,) = helper({"command": "album", "browse_id": "../../x"})
    assert line["error"]["kind"] == "internal"


def test_fetch_reports_progress_then_the_file(helper, tmp_path):
    staging = tmp_path / "t1"
    staging.mkdir()
    lines = helper(
        {"command": "fetch", "video_id": "MdzF0rFOC9Y", "staging_dir": str(staging), "min_abr": 96}
    )
    *progress, done = lines
    assert [p["progress"]["downloaded"] for p in progress] == [1024, 10241]
    assert done == {
        "done": {
            "path": str(staging / "MdzF0rFOC9Y.opus"),
            "format_id": "251",
            "acodec": "opus",
            "abr": 136.374,
            "asr": 48000,
            "duration": 125,
        }
    }


def test_fetch_asks_for_opus_of_96_kbps_from_youtube_only_through_deno(helper, tmp_path, tools):
    staging = tmp_path / "t1"
    staging.mkdir()
    helper({"command": "fetch", "video_id": "MdzF0rFOC9Y", "staging_dir": str(staging), "min_abr": 96})
    options = json.loads((staging / "options.json").read_text())
    assert options["format"] == "bestaudio[acodec=opus][abr>=96]"
    assert options["allowed_extractors"] == ["youtube"]
    assert options["js_runtimes"] == {"deno": {"path": tools["deno"]}}
    assert options["ffmpeg_location"] == tools["ffmpeg"]
    assert options["postprocessors"] == [{"key": "FFmpegExtractAudio", "preferredcodec": "opus"}]
    assert options["paths"] == {"home": str(staging), "temp": str(staging)}
    assert "remote_components" not in options
    assert options["noplaylist"] is True


@pytest.mark.parametrize(
    "video_id, kind",
    [
        ("RATELIMIT01", "rate_limited"),
        ("BOTCHECK001", "bot_check"),
        ("AGECHECK001", "unavailable"),
        ("FORBIDDEN01", "forbidden"),
        ("GONEVIDEO01", "unavailable"),
        ("NOFORMAT001", "format_missing"),
        ("NETWORK0001", "network"),
        ("CUTSHORT001", "network"),
        ("STRANGE0001", "internal"),
    ],
)
def test_each_yt_dlp_failure_is_its_kind_and_keeps_no_url(helper, tmp_path, video_id, kind):
    staging = tmp_path / "t1"
    staging.mkdir()
    (line,) = helper(
        {"command": "fetch", "video_id": video_id, "staging_dir": str(staging), "min_abr": 96}
    )
    assert line["error"]["kind"] == kind
    assert "https://" not in line["error"]["message"]


def test_fetch_without_deno_or_ffmpeg_is_tooling(helper, tmp_path):
    staging = tmp_path / "t1"
    staging.mkdir()
    for missing in ({"deno": str(tmp_path / "no-deno")}, {"ffmpeg": None}):
        (line,) = helper(
            {"command": "fetch", "video_id": "MdzF0rFOC9Y", "staging_dir": str(staging), "min_abr": 96},
            **missing,
        )
        assert line["error"]["kind"] == "tooling", missing


def test_fetch_of_a_video_id_that_is_not_one_is_refused(helper, tmp_path):
    (line,) = helper(
        {"command": "fetch", "video_id": "--exec=rm", "staging_dir": str(tmp_path), "min_abr": 96}
    )
    assert line["error"]["kind"] == "internal"


def test_without_yt_dlp_a_fetch_is_tooling(helper, tmp_path):
    empty = tmp_path / "site-packages"
    empty.mkdir()
    (line,) = helper(
        {"command": "fetch", "video_id": "MdzF0rFOC9Y", "staging_dir": str(tmp_path), "min_abr": 96},
        site_packages=empty,
    )
    assert line["error"]["kind"] == "tooling"


def test_probe_asks_for_the_first_bytes_only(helper, tmp_path):
    staging = tmp_path / "probe"
    staging.mkdir()
    assert helper({"command": "probe", "video_id": "MdzF0rFOC9Y", "staging_dir": str(staging)}) == [
        {"probe": "ok"}
    ]
    options = json.loads((staging / "options.json").read_text())
    assert options["test"] is True


def test_nothing_but_answers_reaches_stdout(helper, tmp_path):
    # The fake's hooks run in the helper; anything it printed would break the protocol.
    staging = tmp_path / "t1"
    staging.mkdir()
    for line in helper(
        {"command": "fetch", "video_id": "MdzF0rFOC9Y", "staging_dir": str(staging), "min_abr": 96}
    ):
        assert set(line) <= {"progress", "done"}
