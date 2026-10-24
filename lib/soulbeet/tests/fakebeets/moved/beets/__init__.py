# A beets the import hook check must refuse although it has everything else:
# import_files lives in a submodule and the package only re-exports it, so
# it would not see the session import.py swaps in the package.
__version__ = "2.99.0"
