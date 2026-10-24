# Loads a beets config the way `beet -c CFG` does (beets.ui._setup: _configure,
# then load_plugins) and prints one JSON line describing the result.
#
# Run as `python -P -c <this file> CFG`, with the same env and cwd as beet.
# Output keys: fatal, loaded, plugin_errors, metadata_sources,
# user_config_path, pluginpath, autotag, import_hook.
import json
import logging
import optparse
import os
import sys
from pathlib import Path

PLUGIN_ERROR_PREFIX = "** error loading plugin "
# soulbeet caps output lines at 8 KiB; the report must fit in one.
MAX_LINE = 8000
FATAL_CAP = 2000

# Plugins prompt on stdout (discogs OAuth), and a plugin's child process writes
# to fd 1, so fd 1 goes to stderr for the whole load and the report gets a
# private copy of the real stdout.
report_out = os.fdopen(os.dup(1), "w")
os.dup2(2, 1)
sys.stdout = sys.stderr


def describe(exc):
    text = str(exc)
    return f"{type(exc).__name__}: {text}" if text else type(exc).__name__


plugin_errors = []


class PluginErrorCapture(logging.Handler):
    def emit(self, record):
        try:
            message = record.getMessage()
            if not message.startswith(PLUGIN_ERROR_PREFIX):
                return
            exc = record.exc_info[1] if record.exc_info else None
            if exc is None:
                error = "unknown error"
            else:
                # PluginImportError wraps the real import failure.
                if type(exc).__name__ == "PluginImportError" and exc.__cause__:
                    exc = exc.__cause__
                error = describe(exc)
            plugin_errors.append(
                {"plugin": message[len(PLUGIN_ERROR_PREFIX):], "error": error}
            )
        except Exception:
            # An exception here would escape load_plugins.
            pass


report = {
    "fatal": None,
    "loaded": [],
    "plugin_errors": plugin_errors,
    "metadata_sources": [],
    "user_config_path": None,
    "pluginpath": [],
    "autotag": True,
    "import_hook": False,
}

try:
    import confuse

    try:
        import beets.ui as ui
        from beets import config, metadata_plugins, plugins

        logging.getLogger("beets").addHandler(PluginErrorCapture())
        ui._configure(optparse.Values({"config": sys.argv[1]}))
        plugins.load_plugins()
    except confuse.ConfigError as exc:
        report["fatal"] = f"configuration error: {exc}"
except BaseException as exc:
    report["fatal"] = describe(exc)

# Each field is read on its own: after a fatal error some can still be known,
# and the base config path matters most when the base config is what failed.
try:
    report["user_config_path"] = config.user_config_path()
except BaseException:
    pass
try:
    report["loaded"] = [p.name for p in plugins.find_plugins()]
    report["metadata_sources"] = [
        p.data_source for p in metadata_plugins.find_metadata_source_plugins()
    ]
except BaseException:
    pass


def import_hook():
    """Whether this beets has what import.py hooks into: an import session
    built from a module global it can replace (beets 2.6), and an id on every
    match (2.8). probe.py and import.py each hold this function, and a test
    keeps the two equal."""
    from beets.autotag.hooks import AlbumInfo, TrackInfo
    from beets.ui.commands import import_
    from beets.ui.commands.import_.session import TerminalImportSession

    return (
        import_.TerminalImportSession is TerminalImportSession
        and "TerminalImportSession" in import_.import_files.__code__.co_names
        # Looked up in the package itself, where import.py swaps it.
        and import_.import_files.__globals__ is vars(import_)
        and all(
            isinstance(getattr(info, "id", None), property)
            for info in (AlbumInfo, TrackInfo)
        )
    )


try:
    report["import_hook"] = import_hook()
except BaseException:
    pass


def read_field(key, read):
    """Stores read() in the report. A failure is fatal only when nothing
    failed before it, so the first cause stays the reported one."""
    try:
        report[key] = read()
    except BaseException as exc:
        if report["fatal"] is None:
            report["fatal"] = (
                f"configuration error: {exc}"
                if isinstance(exc, confuse.ConfigError)
                else describe(exc)
            )


# Resolved the way beets resolves them, against the cwd. Read even after a
# fatal error, so soulbeet notices when a pluginpath dir changes.
read_field(
    "pluginpath",
    lambda: [
        str(Path(p).expanduser().absolute())
        for p in config["pluginpath"].as_str_seq(split=False)
    ],
)
if report["fatal"] is None:
    read_field("autotag", lambda: config["import"]["autotag"].get(bool))


def encode(cap):
    shown = dict(report)
    if shown["fatal"] is not None:
        shown["fatal"] = shown["fatal"][:FATAL_CAP]
    shown["plugin_errors"] = [
        {"plugin": e["plugin"], "error": e["error"][:cap]} for e in plugin_errors
    ]
    return json.dumps(shown)


for cap in (500, 200, 80, 0):
    line = encode(cap)
    if len(line) <= MAX_LINE:
        break

report_out.write(line + "\n")
report_out.flush()
