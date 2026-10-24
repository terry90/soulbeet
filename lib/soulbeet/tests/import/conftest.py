import json
import os
import subprocess
import sys
from pathlib import Path

import pytest
import yaml
from mutagen.easyid3 import EasyID3
from mutagen.id3 import ID3

HERE = Path(__file__).resolve().parent
CRATE = HERE.parents[1]
DRIVER_SRC = (CRATE / "src" / "beets" / "import.py").read_text()
CAPTURE = CRATE / "fixtures" / "beets" / "capture"

# One silent MPEG-1 Layer III frame: 128 kbps, 44.1 kHz, mono, no CRC.
# 77 of them (1152 samples each) last just over two seconds.
MP3_FRAME = b"\xff\xfb\x90\xc4" + bytes(413)
TWO_SECONDS = 77


def strict_json(raw):
    """Parses the report the way serde_json reads it in soulbeet: strict UTF-8,
    and no lone surrogates behind \\u escapes."""
    report = json.loads(raw.decode("utf-8"))
    json.dumps(report, ensure_ascii=False).encode("utf-8")
    return report


class Beets:
    """One isolated beets setup, run the way soulbeet runs it."""

    @staticmethod
    def mp3(path, **tags):
        """A two-second mp3 carrying these ID3 tags (EasyID3 names)."""
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(MP3_FRAME * TWO_SECONDS)
        ID3().save(path)
        if tags:
            easy = EasyID3(path)
            easy.update(tags)
            easy.save()
        return path

    @classmethod
    def album(cls, folder, n=2):
        """n tracks named and tagged like the capture fixtures' albums."""
        return [
            cls.mp3(
                folder / f"0{i} - Song Number {i}.mp3",
                artist="Zzqx Artist",
                albumartist="Zzqx Artist",
                album="Qzz Album",
                title=f"Song Number {i}",
                tracknumber=f"{i}/{n}",
            )
            for i in range(1, n + 1)
        ]

    @staticmethod
    def tags(path):
        return {k: v[0] for k, v in EasyID3(path).items()}

    @staticmethod
    def files_under(root):
        return sorted(
            str(p.relative_to(root)) for p in root.rglob("*") if p.is_file()
        )

    def __init__(self, root):
        root.mkdir()
        self.root = root
        self.home = root / "home"
        self.beetsdir = root / "beetsdir"
        self.music = root / "music"
        self.src = root / "src"
        for d in (self.home, self.beetsdir, self.music, self.src):
            d.mkdir()
        self.config = root / "config.yaml"
        self.configure()
        self.runs = 0

    def configure(self, plugins=("fakesrc",)):
        """The capture fixtures' base config with these plugins."""
        cfg = yaml.safe_load((CAPTURE / "configs" / "base.yaml").read_text())
        cfg["plugins"] = " ".join(plugins)
        cfg["pluginpath"] = [str(CAPTURE), str(HERE / "plugins")]
        self.config.write_text(yaml.safe_dump(cfg))

    def beet_args(self, *sources, singleton=False):
        args = [
            "-v",
            "-c", str(self.config),
            "-l", str(self.music / ".beets_library.db"),
            "-d", str(self.music),
            "import", "-q", "--noincremental", "--noresume",
            "--log", str(self.runlog),
        ]
        if singleton:
            args.append("-s")
        return args + [str(s) for s in (sources or (self.src,))]

    @property
    def runlog(self):
        return self.root / f"runlog{self.runs}.txt"

    def _env(self, extra=None):
        env = dict(os.environ)
        env.update(
            HOME=str(self.home),
            BEETSDIR=str(self.beetsdir),
            NO_COLOR="1",
            PYTHONUNBUFFERED="1",
        )
        env.update(extra or {})
        return env

    def _run(self, argv, env):
        proc = subprocess.run(
            argv,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            env=self._env(env),
            cwd=self.root,
            timeout=120,
        )
        runlog = self.runlog.read_text() if self.runlog.exists() else ""
        self.runs += 1
        return Run(proc, runlog)

    def driver(self, *choice, sources=(), singleton=False, env=None):
        """Runs import.py and returns the Run with its parsed report."""
        report = self.root / f"report{self.runs}.json"
        args = self.beet_args(*sources, singleton=singleton)
        argv = [sys.executable, "-P", "-c", DRIVER_SRC, str(report), *choice]
        run = self._run([*argv, "--", *args], env)
        run.report = strict_json(report.read_bytes()) if report.exists() else None
        return run

    def plain(self, *sources, singleton=False, env=None):
        """Runs beet itself with the same arguments."""
        args = self.beet_args(*sources, singleton=singleton)
        return self._run([sys.executable, "-P", "-m", "beets", *args], env)

    def library(self):
        """(title, artist, album, mb_trackid, path relative to music) per item."""
        out = subprocess.run(
            [
                sys.executable, "-P", "-m", "beets",
                "-c", str(self.config),
                "-l", str(self.music / ".beets_library.db"),
                "-d", str(self.music),
                "ls", "-f", "$title\t$artist\t$album\t$mb_trackid\t$path",
            ],
            capture_output=True,
            text=True,
            env=self._env(),
            cwd=self.root,
            check=True,
        ).stdout
        rows = []
        for line in out.splitlines():
            title, artist, album, trackid, path = line.split("\t")
            relative = str(Path(path).relative_to(self.music))
            rows.append((title, artist, album, trackid, relative))
        return sorted(rows)

    def music_files(self):
        return [f for f in self.files_under(self.music) if f != ".beets_library.db"]


class Run:
    def __init__(self, proc, runlog):
        self.code = proc.returncode
        self.stdout = proc.stdout
        self.stderr = proc.stderr
        # The header line carries the start time.
        self.runlog_lines = [
            line
            for line in runlog.splitlines()
            if not line.startswith("import started ")
        ]
        self.report = None

    def __repr__(self):
        return f"exit {self.code}\nstdout:\n{self.stdout}\nstderr:\n{self.stderr}"


@pytest.fixture
def beets_env(tmp_path):
    return Beets(tmp_path / "env")


@pytest.fixture
def plain_env(tmp_path):
    """A second setup laid out like beets_env, for runs of beet itself."""
    return Beets(tmp_path / "plain")
