import os
import shlex
from pathlib import Path

import pytest
import yaml

# Defined here, not imported from conftest: with tests/import in the same run,
# that module name is ambiguous.
HERE = Path(__file__).resolve().parent
CONFIGS = HERE / "configs"
REPO = HERE.parents[3]
REPO_PLUGINS = REPO / "beets_plugins"
FAKE_BEETS = HERE.parent / "fakebeets"


def errors_by_plugin(report):
    return {e["plugin"]: e["error"] for e in report["plugin_errors"]}


def tier_plugins(tier):
    env = (REPO / "build" / "tiers" / f"{tier}.env").read_text()
    for line in env.splitlines():
        if line.startswith("BEETS_PLUGINS="):
            return shlex.split(line.split("=", 1)[1])[0]
    raise AssertionError(f"no BEETS_PLUGINS in {tier}.env")


def test_shipped_config_is_clean(run_probe, tmp_path):
    cfg = yaml.safe_load((REPO / "beets_config.yaml").read_text())
    # Same templating as the Dockerfile does for the light tier.
    cfg["plugins"] = tier_plugins("light")
    cfg["pluginpath"] = [str(REPO_PLUGINS)]
    path = tmp_path / "shipped.yaml"
    path.write_text(yaml.safe_dump(cfg))

    report = run_probe(path)
    assert report["fatal"] is None
    assert "musicbrainz" in report["loaded"]
    assert "mbtwopass" in report["loaded"]
    assert report["plugin_errors"] == []
    assert "MusicBrainz" in report["metadata_sources"]
    assert report["pluginpath"] == [str(REPO_PLUGINS)]
    assert report["autotag"] is True
    assert report["import_hook"] is True
    assert report["user_config_path"] == str(run_probe.beetsdir / "config.yaml")


def test_empty_config_keeps_beets_default_plugins(run_probe):
    report = run_probe(CONFIGS / "empty.yaml")
    assert report["fatal"] is None
    assert report["metadata_sources"] == ["MusicBrainz"]


def test_missing_config_is_fatal(run_probe, tmp_path):
    report = run_probe(tmp_path / "missing.yaml")
    assert report["fatal"].startswith("configuration error:")


def test_user90_config_has_no_usable_source(run_probe):
    report = run_probe(CONFIGS / "user90.yaml")
    assert report["fatal"] is None
    # chroma loads, but it looks up through MusicBrainz, which is not loaded.
    assert report["metadata_sources"] == ["Acoustid"]
    assert "not recognized" in errors_by_plugin(report)["fetchart"]


def test_musicbrainz_with_mbpseudo_is_fatal(run_probe):
    report = run_probe(CONFIGS / "mbpseudo.yaml")
    assert report["fatal"] is not None


@pytest.mark.skipif(os.geteuid() == 0, reason="root reads files regardless of mode")
def test_unreadable_base_config_is_fatal(run_probe):
    base = run_probe.beetsdir / "config.yaml"
    base.write_text("plugins: musicbrainz\n")
    base.chmod(0)
    try:
        report = run_probe(CONFIGS / "base_ok.yaml")
    finally:
        base.chmod(0o644)
    assert report["fatal"] is not None


def test_mbtwopass_without_pluginpath_fails_to_load(run_probe):
    report = run_probe(CONFIGS / "mbtwopass.yaml")
    assert "No module named" in errors_by_plugin(report)["mbtwopass"]


def test_mbtwopass_with_pluginpath_loads(run_probe, tmp_path):
    path = tmp_path / "mbtwopass.yaml"
    path.write_text(
        (CONFIGS / "mbtwopass.yaml").read_text()
        + yaml.safe_dump({"pluginpath": [str(REPO_PLUGINS)]})
    )
    report = run_probe(path)
    assert report["fatal"] is None
    assert report["plugin_errors"] == []
    assert "mbtwopass" in report["loaded"]


def test_chroma_alone_reports_only_acoustid(run_probe):
    report = run_probe(CONFIGS / "chroma.yaml")
    assert report["metadata_sources"] == ["Acoustid"]


def test_discogs_without_token_still_gives_one_json_line(run_probe):
    report = run_probe(
        CONFIGS / "discogs.yaml", env={"HTTPS_PROXY": "http://127.0.0.1:9"}
    )
    assert "discogs" in errors_by_plugin(report)


def test_a_plugin_prompting_on_stdout_does_not_corrupt_the_report(run_probe, tmp_path):
    path = tmp_path / "noisy.yaml"
    path.write_text(
        yaml.safe_dump(
            {"plugins": ["musicbrainz", "noisy"], "pluginpath": [str(HERE / "plugins")]}
        )
    )
    report = run_probe(path)
    assert report["fatal"] is None
    assert "stdin stream ended" in errors_by_plugin(report)["noisy"]


def test_autotag_off_is_reported(run_probe):
    report = run_probe(CONFIGS / "autotag_off.yaml")
    assert report["autotag"] is False


def test_pluginpath_is_reported_after_a_fatal_error(run_probe, tmp_path):
    path = tmp_path / "fatal_with_pluginpath.yaml"
    path.write_text(
        (CONFIGS / "mbpseudo.yaml").read_text()
        + yaml.safe_dump({"pluginpath": [str(REPO_PLUGINS)]})
    )
    report = run_probe(path)
    assert report["fatal"] is not None
    assert report["pluginpath"] == [str(REPO_PLUGINS)]


def test_a_bad_value_read_after_load_is_a_configuration_error(run_probe, tmp_path):
    path = tmp_path / "bad_autotag.yaml"
    path.write_text(yaml.safe_dump({"plugins": "musicbrainz", "import": {"autotag": [1, 2]}}))
    report = run_probe(path)
    assert report["fatal"].startswith("configuration error:"), report["fatal"]


def test_import_hook_is_reported_after_a_fatal_error(run_probe, tmp_path):
    report = run_probe(tmp_path / "missing.yaml")
    assert report["fatal"] is not None
    assert report["import_hook"] is True


@pytest.mark.parametrize("layout", ["1.6", "2.6", "moved"])
def test_beets_without_the_hook_has_no_import_hook(run_probe, layout):
    report = run_probe(CONFIGS / "empty.yaml", env={"PYTHONPATH": str(FAKE_BEETS / layout)})
    assert report["import_hook"] is False
