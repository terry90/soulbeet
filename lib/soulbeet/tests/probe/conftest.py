import json
import os
import subprocess
import sys
from pathlib import Path

import pytest

CRATE = Path(__file__).resolve().parents[2]
PROBE_SRC = (CRATE / "src" / "beets" / "probe.py").read_text()


@pytest.fixture
def run_probe(tmp_path):
    """Runs probe.py the way soulbeet does and returns the parsed report."""
    home = tmp_path / "home"
    beetsdir = tmp_path / "beetsdir"
    home.mkdir()
    beetsdir.mkdir()

    def run(cfg, env=None):
        full_env = dict(os.environ)
        full_env.update(
            HOME=str(home),
            BEETSDIR=str(beetsdir),
            NO_COLOR="1",
            PYTHONUNBUFFERED="1",
            # Keep plugins that call out at load time off the network.
            HTTP_PROXY="http://127.0.0.1:9",
            HTTPS_PROXY="http://127.0.0.1:9",
        )
        full_env.update(env or {})
        proc = subprocess.run(
            [sys.executable, "-P", "-c", PROBE_SRC, str(cfg)],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            env=full_env,
            cwd=tmp_path,
            timeout=60,
        )
        lines = proc.stdout.splitlines()
        assert len(lines) == 1, f"stdout: {proc.stdout!r}\nstderr: {proc.stderr}"
        return json.loads(lines[0])

    run.beetsdir = beetsdir
    return run
