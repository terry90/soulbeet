// An MCP client as an app would run it: the official TypeScript SDK signs in
// over OAuth (dynamic registration, PKCE) while Playwright drives the real
// login and consent pages in between, then calls tools.

import { randomUUID } from 'node:crypto';
import type { Page } from '@playwright/test';
import { expect } from '@playwright/test';
import { UnauthorizedError } from '@modelcontextprotocol/sdk/client/auth.js';
import type { OAuthClientProvider } from '@modelcontextprotocol/sdk/client/auth.js';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StreamableHTTPClientTransport } from '@modelcontextprotocol/sdk/client/streamableHttp.js';
import type {
  OAuthClientInformationMixed,
  OAuthClientMetadata,
  OAuthTokens,
} from '@modelcontextprotocol/sdk/shared/auth.js';
import { CallToolResultSchema } from '@modelcontextprotocol/sdk/types.js';
import type { Tool } from '@modelcontextprotocol/sdk/types.js';
import type { TestUser } from './app.js';
import { mcpBaseUrl } from './env.js';

export const mcpUrl = `${mcpBaseUrl}/mcp`;

/** Where the app asks to be sent back: loopback, and nothing listens there. */
export const CALLBACK = 'http://127.0.0.1:8976/callback';

const PROTOCOL_VERSION = '2025-11-25';

export const INITIALIZE = {
  jsonrpc: '2.0',
  id: 1,
  method: 'initialize',
  params: {
    protocolVersion: PROTOCOL_VERSION,
    capabilities: {},
    clientInfo: { name: 'soulful-acceptance', version: '1' },
  },
};

/**
 * POST to /mcp with fetch: content-type JSON, accept "application/json,
 * text/event-stream", mcp-protocol-version 2025-11-25 unless `headers` names
 * another, Authorization: Bearer when `token` is given.
 */
export function rawPost(
  token: string | null,
  body: unknown,
  headers: Record<string, string> = {},
): Promise<Response> {
  return fetch(mcpUrl, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      accept: 'application/json, text/event-stream',
      'mcp-protocol-version': PROTOCOL_VERSION,
      ...(token === null ? {} : { authorization: `Bearer ${token}` }),
      ...headers,
    },
    body: JSON.stringify(body),
  });
}

/** Keeps what the SDK hands it in memory, for one sign-in. */
export class TestOAuthProvider implements OAuthClientProvider {
  private readonly stateValue = randomUUID();
  private client: OAuthClientInformationMixed | undefined;
  private saved: OAuthTokens | undefined;
  private verifier: string | undefined;
  private lastAuthorizationUrl: URL | undefined;

  get redirectUrl(): string {
    return CALLBACK;
  }

  get clientMetadata(): OAuthClientMetadata {
    return {
      client_name: 'Soulful acceptance',
      redirect_uris: [CALLBACK],
      grant_types: ['authorization_code'],
      response_types: ['code'],
      token_endpoint_auth_method: 'none',
    };
  }

  state(): string {
    return this.stateValue;
  }

  clientInformation(): OAuthClientInformationMixed | undefined {
    return this.client;
  }

  saveClientInformation(clientInformation: OAuthClientInformationMixed): void {
    this.client = clientInformation;
  }

  tokens(): OAuthTokens | undefined {
    return this.saved;
  }

  saveTokens(tokens: OAuthTokens): void {
    this.saved = tokens;
  }

  redirectToAuthorization(authorizationUrl: URL): void {
    this.lastAuthorizationUrl = authorizationUrl;
  }

  saveCodeVerifier(codeVerifier: string): void {
    this.verifier = codeVerifier;
  }

  codeVerifier(): string {
    if (this.verifier === undefined) throw new Error('the SDK saved no code verifier');
    return this.verifier;
  }

  /** The URL redirectToAuthorization was handed last. */
  authorizationUrl(): URL {
    if (this.lastAuthorizationUrl === undefined) throw new Error('the SDK opened no authorization URL');
    return this.lastAuthorizationUrl;
  }

  /** The value state() gives. */
  expectedState(): string {
    return this.stateValue;
  }

  /** The access token saveTokens stored last. */
  accessToken(): string {
    if (this.saved === undefined) throw new Error('the SDK saved no tokens');
    return this.saved.access_token;
  }
}

export interface ToolAnswer {
  isError: boolean;
  doc: Record<string, unknown>;
}

export interface McpSession {
  readonly user: TestUser;
  readonly provider: TestOAuthProvider;
  /** tools/call with a 240 s timeout; checks one text block and no structuredContent, and parses it. */
  call(name: string, args?: Record<string, unknown>): Promise<ToolAnswer>;
  /** call, failing the test with the document when isError. */
  ok(name: string, args?: Record<string, unknown>): Promise<Record<string, unknown>>;
  listTools(): Promise<Tool[]>;
  close(): Promise<void>;
}

function session(client: Client, user: TestUser, provider: TestOAuthProvider): McpSession {
  const call = async (name: string, args: Record<string, unknown> = {}): Promise<ToolAnswer> => {
    // callTool's type admits the pre-2024-11-05 `toolResult` answer; this pins the current one.
    const result = CallToolResultSchema.parse(
      await client.callTool({ name, arguments: args }, undefined, { timeout: 240_000 }),
    );
    const [block, ...more] = result.content;
    if (block?.type !== 'text' || more.length > 0) {
      throw new Error(`${name} answered ${JSON.stringify(result.content)}, not one text block`);
    }
    expect(result.structuredContent, name).toBeUndefined();
    expect(typeof result.isError, `${name} isError`).toBe('boolean');
    return { isError: result.isError === true, doc: JSON.parse(block.text) as Record<string, unknown> };
  };
  return {
    user,
    provider,
    call,
    async ok(name, args) {
      const answer = await call(name, args);
      expect(answer.isError, `${name} failed:\n${JSON.stringify(answer.doc, null, 2)}`).toBe(false);
      return answer.doc;
    },
    async listTools() {
      const { tools, nextCursor } = await client.listTools();
      expect(nextCursor, 'tools/list in one page').toBeUndefined();
      return tools;
    },
    close: () => client.close(),
  };
}

/**
 * Connects as `user` through `page`, whose context has no Soulbeet session:
 * the SDK's first connect registers and asks for authorization, the page
 * signs in and allows, and the SDK exchanges the code the app sent back.
 */
export async function connectAs(page: Page, user: TestUser): Promise<McpSession> {
  const provider = new TestOAuthProvider();
  const first = new StreamableHTTPClientTransport(new URL(mcpUrl), { authProvider: provider });
  await expect(new Client({ name: 'soulful-acceptance', version: '1' }).connect(first)).rejects.toBeInstanceOf(
    UnauthorizedError,
  );

  await page.goto(provider.authorizationUrl().href);
  await expect(page).toHaveURL(/\/login\/return\?return_to=/);
  expect(new URL(page.url()).searchParams.get('return_to')).toMatch(/^\/oauth\/authorize\?/);
  await page.getByPlaceholder('Enter username').fill(user.username);
  await page.getByPlaceholder('Enter password').fill(user.password);
  await page.getByRole('button', { name: 'AUTHENTICATE' }).click();
  await expect(page.getByRole('heading', { name: 'Connect 127.0.0.1 to Soulful' })).toBeVisible();
  await expect(page.getByText(`127.0.0.1 wants to use Soulful as ${user.username}.`)).toBeVisible();
  await expect(page.getByText('It calls itself "Soulful acceptance".')).toBeVisible();

  // The app's 303 sends the browser to the callback. A route handler sees
  // only the first URL of a redirect, so the callback is read from the
  // request the browser makes; nothing answers it.
  const sentBack = page.waitForRequest((request) => request.url().startsWith(CALLBACK));
  await page.getByRole('button', { name: 'Allow', exact: true }).click();
  const callback = new URL((await sentBack).url());
  expect([callback.searchParams.get('state'), callback.searchParams.get('iss')]).toEqual([
    provider.expectedState(),
    mcpBaseUrl,
  ]);
  const code = callback.searchParams.get('code');
  if (code === null) throw new Error(`the callback carries no code: ${callback.href}`);

  // The transport whose connect failed holds the challenge's metadata URL and scope.
  await first.finishAuth(code);
  const client = new Client({ name: 'soulful-acceptance', version: '1' });
  await client.connect(new StreamableHTTPClientTransport(new URL(mcpUrl), { authProvider: provider }));
  return session(client, user, provider);
}
