"""Writes to stdout every way a plugin can, then prompts like discogs does."""

import os
import subprocess

from beets import ui
from beets.plugins import BeetsPlugin


class NoisyPlugin(BeetsPlugin):
    def __init__(self):
        super().__init__()
        print("print to stdout")
        os.write(1, b"raw write to fd 1\n")
        subprocess.run(["echo", "child process output"])
        ui.input_("Enter the code:")
