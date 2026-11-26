import json
import os
import subprocess
import sys
from pathlib import Path

import pytest

HERE = Path(__file__).resolve().parent
CRATE = HERE.parents[1]
HELPER = (CRATE / "src" / "ytdlp" / "helper.py").read_text()
FIXTURES = CRATE / "src" / "ytdlp" / "fixtures"
FAKES = HERE / "fakes"


def executable(path, text):
    path.write_text(text)
    path.chmod(0o755)
    return path


@pytest.fixture
def tools(tmp_path):
    """A deno and an ffmpeg that print their versions as the real ones do."""
    deno = executable(
        tmp_path / "deno",
        "#!/bin/sh\necho 'deno 2.9.8 (stable, release, x86_64-unknown-linux-gnu)'\n",
    )
    ffmpeg = executable(
        tmp_path / "ffmpeg",
        "#!/bin/sh\necho 'ffmpeg version 8.0.1 Copyright (c) 2000-2025 the FFmpeg developers'\n",
    )
    return {"deno": str(deno), "ffmpeg": str(ffmpeg)}


@pytest.fixture
def home(tmp_path):
    home = tmp_path / "home"
    home.mkdir()
    return home


@pytest.fixture
def spawn(tmp_path, tools, home):
    """Starts the helper as Soulbeet does, on the fakes: the leader of its own process group,
    its environment only its own folder, its request on stdin, which stays open."""
    started = []

    def start(request, site_packages=FAKES, **overrides):
        envelope = {
            "site_packages": str(site_packages),
            "cache_dir": str(tmp_path / "cache"),
            **tools,
            **overrides,
            **request,
        }
        said = tmp_path / f"stderr-{len(started)}"
        with said.open("w") as stderr:
            process = subprocess.Popen(
                [sys.executable, "-I", "-X", "utf8", "-c", HELPER],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=stderr,
                text=True,
                env={"HOME": str(home), "TMPDIR": str(home), "LANG": "C.UTF-8"},
                cwd=home,
                process_group=0,
            )
        process.said = said
        started.append(process)
        process.stdin.write(json.dumps(envelope) + "\n")
        process.stdin.flush()
        return process

    yield start
    for process in started:
        if process.poll() is None:
            process.kill()
            process.wait()


@pytest.fixture
def helper(spawn, tmp_path):
    """Runs the helper as Soulbeet does, and closes its stdin once it ended. Returns the lines
    it printed."""

    def run(request, **overrides):
        process = spawn(request, **overrides)
        out = process.stdout.read()
        ended = process.wait(timeout=60)
        process.stdin.close()
        assert ended == 0, process.said.read_text()
        return [json.loads(line) for line in out.splitlines()]

    return run


def fixture_lines(name):
    return [json.loads(line) for line in (FIXTURES / f"{name}.ndjson").read_text().splitlines()]
