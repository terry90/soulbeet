from .session import TerminalImportSession


def import_files(lib, paths, query):
    TerminalImportSession(lib, None, paths, query).run()
