// Acts on the suite's own compose stack: restarts the MCP instance, and only
// that one service of the suite's compose project.

import { execFileSync } from 'node:child_process';
import { expect } from '@playwright/test';
import { e2eRoot, mcpBaseUrl, mcpCompose } from './env.js';

/**
 * Restarts the soulbeet-mcp service with the compose command E2E_MCP_COMPOSE
 * names, then waits until the instance answers its protected resource
 * metadata again.
 */
export async function restartMcpInstance(): Promise<void> {
  if (mcpCompose === undefined) {
    throw new Error('E2E_MCP_COMPOSE is not set: the restart needs the compose command of the MCP stack');
  }
  const words = mcpCompose.split(/\s+/).filter((word) => word.length > 0);
  const [command, ...args] = words;
  if (command === undefined) throw new Error('E2E_MCP_COMPOSE is empty');
  execFileSync(command, [...args, 'restart', 'soulbeet-mcp'], { cwd: e2eRoot, stdio: 'inherit' });
  await expect
    .poll(
      async () => {
        try {
          return (await fetch(`${mcpBaseUrl}/.well-known/oauth-protected-resource/mcp`)).status;
        } catch {
          // Refused while the instance starts.
          return 0;
        }
      },
      { timeout: 120_000 },
    )
    .toBe(200);
}
