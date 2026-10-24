// node:sqlite prints an ExperimentalWarning in every worker that loads it.
// The beets library reads in files.ts need nothing that may change, so that
// one warning is dropped while the module loads. Every other warning still
// prints.

import type * as Sqlite from 'node:sqlite';

type EmitWarning = typeof process.emitWarning;

const emitWarning: EmitWarning = process.emitWarning;

function isSqliteWarning(warning: string | Error, rest: unknown[]): boolean {
  const [kind] = rest;
  const type = typeof kind === 'object' && kind !== null ? (kind as { type?: string }).type : kind;
  return type === 'ExperimentalWarning' && String(warning).startsWith('SQLite ');
}

process.emitWarning = function (warning: string | Error, ...rest: unknown[]) {
  if (!isSqliteWarning(warning, rest)) {
    (emitWarning as (...args: unknown[]) => void).call(process, warning, ...rest);
  }
} as EmitWarning;

let sqlite: typeof Sqlite;
try {
  sqlite = process.getBuiltinModule('node:sqlite');
} finally {
  process.emitWarning = emitWarning;
}

export const { DatabaseSync } = sqlite;
