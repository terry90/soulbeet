"""The tags beets writes in a driver run, read back with mutagen alone."""

import mutagen
import pytest
from mutagen.id3 import ID3, TIPL, TXXX, Frames
from mutagen.mp4 import MP4FreeForm

STRONG = {"FAKESRC_MODE": "strong"}

VORBIS = {
    "title": "title",
    "artist": "artist",
    "track": "tracknumber",
    "date": "date",
    "original_date": "originaldate",
    "bpm": "bpm",
    "arranger": "arranger",
    "grouping": "grouping",
    "initial_key": "initialkey",
    "comp": "compilation",
    "rg_track_gain": "replaygain_track_gain",
    "r128_track_gain": "r128_track_gain",
}

# Each field's key in the file, as mutagen names it.
KEYS = {
    ".mp3": {
        "title": "TIT2",
        "artist": "TPE1",
        "track": "TRCK",
        "date": "TDRC",
        "original_date": "TDOR",
        "bpm": "TBPM",
        "arranger": "TIPL",
        "comp": "TCMP",
        "rg_track_gain": "TXXX:REPLAYGAIN_TRACK_GAIN",
        "r128_track_gain": "TXXX:R128_TRACK_GAIN",
    },
    ".flac": VORBIS,
    ".opus": VORBIS,
    ".m4a": {
        "title": "\xa9nam",
        "artist": "\xa9ART",
        "track": "trkn",
        "date": "\xa9day",
        "original_date": "----:com.apple.iTunes:ORIGINALDATE",
        "bpm": "tmpo",
        "arranger": "----:com.apple.iTunes:Arranger",
        "grouping": "\xa9grp",
        "initial_key": "----:com.apple.iTunes:initialkey",
        "comp": "cpil",
        "rg_track_gain": "----:com.apple.iTunes:replaygain_track_gain",
        "r128_track_gain": "----:com.apple.iTunes:R128_TRACK_GAIN",
    },
}

MATCHED = {"title": "Song Number 1", "artist": "Zzqx Artist"}

# Values with no meaning a tagger leaves behind. mutagen does not save an
# empty ID3 text frame, so the mp3 has no empty grouping or initial key.
PLACEHOLDERS = {
    "original_date": "0000",
    "bpm": "0",
    "arranger": "",
    "grouping": "",
    "initial_key": "",
}

# Real values, among them a year alone (month and day unknown) and zero gains.
REAL = {
    "track": "3",
    "date": "2019",
    "rg_track_gain": "0.00 dB",
    "r128_track_gain": "0",
}

# What a file holds when it holds nothing.
NOTHING = {"", "0", "0000", "0/0"}


def write_raw(path, fields):
    """Writes `fields` (field name -> text) into the file with mutagen."""
    keys = KEYS[path.suffix]
    f = mutagen.File(path)
    for name, value in fields.items():
        key = keys[name]
        if path.suffix == ".mp3":
            if key == "TIPL":
                frame = TIPL(encoding=3, people=[["arranger", value]])
            elif key.startswith("TXXX:"):
                frame = TXXX(encoding=3, desc=key[5:], text=[value])
            else:
                frame = Frames[key](encoding=3, text=[value])
            f.tags.add(frame)
        elif key.startswith("----:"):
            f[key] = [MP4FreeForm(value.encode())]
        elif key == "tmpo":
            f[key] = [int(value)]
        elif key == "trkn":
            f[key] = [(int(value), 0)]
        else:
            f[key] = [value]
    f.save()


def text(value):
    if isinstance(value, bytes):
        return value.decode()
    if isinstance(value, tuple):
        return "/".join(map(str, value))
    if isinstance(value, bool):
        return str(int(value))
    return str(value)


def read_raw(path):
    """Every tag in the file: its mutagen key and its values as text."""
    if path.suffix == ".mp3":
        out = {}
        for frame in ID3(path).values():
            if frame.FrameID == "TIPL":
                values = [name for _, name in frame.people]
            elif isinstance(getattr(frame, "text", None), list):
                values = frame.text
            elif hasattr(frame, "text"):
                values = [frame.text]
            else:
                values = [frame.data]
            out[frame.HashKey] = [text(v) for v in values]
        return out
    f = mutagen.File(path)
    return {
        key: [text(v) for v in (value if isinstance(value, list) else [value])]
        for key, value in f.tags.items()
    }


def imported_file(env):
    [name] = env.music_files()
    return env.music / name


def import_one(env, suffix):
    """Imports one file carrying the real values and the placeholders, and
    returns the run, the tags beets left in the file and their keys."""
    source = env.audio(env.src / f"one{suffix}")
    keys = KEYS[suffix]
    placeholders = {k: v for k, v in PLACEHOLDERS.items() if k in keys}
    write_raw(source, {**MATCHED, **REAL, **placeholders})

    run = env.driver("auto", singleton=True, env=STRONG)
    assert run.code == 0, run
    return run, read_raw(imported_file(env)), keys, placeholders


@pytest.mark.parametrize("suffix", [".mp3", ".flac", ".m4a", ".opus"])
def test_a_written_file_holds_only_tags_with_a_value(beets_env, suffix):
    _, tags, keys, placeholders = import_one(beets_env, suffix)

    expected = {**MATCHED, **REAL}
    if suffix == ".m4a":
        # MP4 keeps the number with its total, 0 when unknown.
        expected["track"] = "3/0"
    for name, value in expected.items():
        assert tags[keys[name]] == [value], (name, tags)
    # beets holds a compilation flag of False as a value.
    assert tags[keys["comp"]] == ["0"], tags
    for name in placeholders:
        assert keys[name] not in tags, (name, tags)
    assert_no_placeholder(tags, keys)


def assert_no_placeholder(tags, keys):
    """Nothing beets has no value for was written."""
    real_zeros = {keys["comp"], keys["r128_track_gain"]}
    for key, values in tags.items():
        if key not in real_zeros:
            assert values and not NOTHING & set(values), (key, tags)


def test_a_plugin_writing_the_file_again_writes_only_tags_with_a_value(beets_env):
    # scrub deletes every tag of an imported file and writes the item again.
    beets_env.configure(plugins=("fakesrc", "scrub"))
    run, tags, keys, _ = import_one(beets_env, ".flac")

    # Once by the import, once by scrub.
    writes = (run.stdout + run.stderr).splitlines().count("Sending event: write")
    assert writes == 2, run
    assert tags[keys["title"]] == [MATCHED["title"]], tags
    assert_no_placeholder(tags, keys)


def test_a_plugin_media_field_is_left_out_only_when_empty(beets_env):
    beets_env.configure(plugins=("fakesrc", "mood"))
    for i, mood in ((1, "calm"), (2, "")):
        source = beets_env.audio(beets_env.src / f"{i}.flac")
        f = mutagen.File(source)
        f["title"] = [f"Song Number {i}"]
        f["artist"] = ["Zzqx Artist"]
        f["mood"] = [mood]
        f.save()

    run = beets_env.driver("auto", singleton=True, env=STRONG)
    assert run.code == 0, run
    moods = {}
    for name in beets_env.music_files():
        tags = read_raw(beets_env.music / name)
        moods[tags["title"][0]] = tags.get("mood")
    assert moods == {"Song Number 1": ["calm"], "Song Number 2": None}
