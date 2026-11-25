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
def helper(tmp_path, tools):
    """Runs the helper as Soulbeet does, on the fakes: its request on stdin, its environment
    only its own folder. Returns the lines it printed."""
    home = tmp_path / "home"
    home.mkdir()
    cache = tmp_path / "cache"

    def run(request, site_packages=FAKES, **overrides):
        envelope = {
            "site_packages": str(site_packages),
            "cache_dir": str(cache),
            **tools,
            **overrides,
            **request,
        }
        done = subprocess.run(
            [sys.executable, "-I", "-X", "utf8", "-c", HELPER],
            input=json.dumps(envelope) + "\n",
            capture_output=True,
            text=True,
            env={"HOME": str(home), "TMPDIR": str(home), "LANG": "C.UTF-8"},
            cwd=home,
            timeout=60,
        )
        assert done.returncode == 0, done.stderr
        return [json.loads(line) for line in done.stdout.splitlines()]

    return run


def fixture_lines(name):
    return [json.loads(line) for line in (FIXTURES / f"{name}.ndjson").read_text().splitlines()]
