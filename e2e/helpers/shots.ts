// Screenshots of the screens a spec reaches, for reviewing the UI. Saved
// only when E2E_SHOTS_DIR is set, so normal runs write nothing.

import { join } from 'node:path';
import type { Locator } from '@playwright/test';
import { shotsDir } from './env.js';

/** Saves `target` as `${E2E_SHOTS_DIR}/<name>.png`, e.g. name "01-skipped-row". */
export async function shot(target: Locator, name: string): Promise<void> {
  if (shotsDir === undefined) {
    return;
  }
  await target.screenshot({ path: join(shotsDir, `${name}.png`) });
}
