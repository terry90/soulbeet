# Drops from one beets library the items whose file is gone, and touches no
# file: beets' own CLI (beets.ui.main) runs the soulbeet-prune command this
# file adds, with no plugin loaded, so no plugin acts on the removals.
#
#   python -P -c <this file> <beet global args> soulbeet-prune
#
# An item's file is gone when looking at its path fails with ENOENT; any other
# failure, such as a folder that cannot be read, keeps the item. An item's file
# is there when its path is a regular file. While some files are gone and no
# item has its file there, nothing is dropped: the library's folder may be a
# share that is not mounted. beets gives each item's path absolute, from a row
# stored relative to the library folder (2.10+) or an absolute one.
#
# stdout carries one line per dropped item, then one line that starts with
# @@sb@@:
#   @@sb@@pruned REMOVED KEPT  the gone items were dropped, KEPT stay
#   @@sb@@unproven ITEMS       some files are gone, none of ITEMS is there
import os
import stat
import sys

import beets.ui
from beets import util
from beets.ui.commands import default_commands

MARKER = "@@sb@@"


def look(path):
    """'gone' when nothing is at `path`, 'there' when a regular file is,
    None when it cannot be told or is something else."""
    syspath = util.syspath(path)
    try:
        os.lstat(syspath)
    except FileNotFoundError:
        return "gone"
    except OSError:
        return None
    try:
        return "there" if stat.S_ISREG(os.stat(syspath).st_mode) else None
    except OSError:
        return None


def prune(lib, opts, args):
    items = list(lib.items())
    looks = [look(item.path) for item in items]
    gone = [item for item, seen in zip(items, looks) if seen == "gone"]
    if gone and "there" not in looks:
        print(f"{MARKER}unproven {len(items)}")
        return
    with lib.transaction():
        for item in gone:
            # beets' remove without its -d: the row goes, with an album left
            # empty, and the file stays.
            item.remove(delete=False, with_album=True)
    for item in gone:
        print(f"soulbeet: dropped {util.displayable_path(item.path)}")
    print(f"{MARKER}pruned {len(gone)} {len(items) - len(gone)}")


command = beets.ui.Subcommand("soulbeet-prune", help="drop the items whose file is gone")
command.func = prune
default_commands.append(command)

# -p with no name loads no plugin, whatever the config lists.
beets.ui.main(["-p", "", *sys.argv[1:]])
