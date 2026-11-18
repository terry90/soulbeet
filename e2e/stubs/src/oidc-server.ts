// OpenID Connect provider double for the single sign-on specs. It does what
// soulbeet relies on and checks what soulbeet must send: discovery, an
// authorization page, the code exchange (client secret, redirect URI and
// PKCE S256) and RS256 ID tokens with the nonce, verifiable through /jwks.
//
// The browser and the app reach the stub on different hosts in compose
// (127.0.0.1 vs stubs), so the discovery document gives the browser-facing
// URL for the authorization endpoint and the app-facing one for the rest.
//
// The authorization page is a form: the spec types who signs in (username and
// subject) and approves or denies, so parallel specs never share state.

import { createHash, generateKeyPairSync, randomUUID, sign } from 'node:crypto';
import type { Server } from 'node:http';
import { createStubServer, sendJson, sendText } from './router.js';

export interface OidcStubOptions {
  /** Issuer, as the app reaches the stub. */
  issuer: string;
  /** Base URL of the stub as the browser reaches it. */
  browserUrl: string;
  clientId: string;
  clientSecret: string;
}

interface PendingCode {
  redirectUri: string;
  codeChallenge: string;
  nonce: string;
  subject: string;
  username: string;
}

const base64url = (data: Buffer | string): string => Buffer.from(data).toString('base64url');

const escapeHtml = (value: string): string =>
  value.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);

export function createOidcServer(options: OidcStubOptions): Server {
  const { privateKey, publicKey } = generateKeyPairSync('rsa', { modulusLength: 2048 });
  const kid = randomUUID();
  const jwk = { ...publicKey.export({ format: 'jwk' }), kid, use: 'sig', alg: 'RS256' };
  const codes = new Map<string, PendingCode>();

  const signIdToken = (claims: Record<string, unknown>): string => {
    const header = base64url(JSON.stringify({ alg: 'RS256', typ: 'JWT', kid }));
    const payload = base64url(JSON.stringify(claims));
    const signature = sign('RSA-SHA256', Buffer.from(`${header}.${payload}`), privateKey);
    return `${header}.${payload}.${base64url(signature)}`;
  };

  return createStubServer({
    name: 'oidc',
    routes: [
      {
        method: 'GET',
        pattern: '/.well-known/openid-configuration',
        handler: ({ res }) =>
          sendJson(res, 200, {
            issuer: options.issuer,
            authorization_endpoint: `${options.browserUrl}/authorize`,
            token_endpoint: `${options.issuer}/token`,
            jwks_uri: `${options.issuer}/jwks`,
            response_types_supported: ['code'],
            subject_types_supported: ['public'],
            id_token_signing_alg_values_supported: ['RS256'],
            token_endpoint_auth_methods_supported: ['client_secret_basic'],
            code_challenge_methods_supported: ['S256'],
          }),
      },
      {
        method: 'GET',
        pattern: '/jwks',
        handler: ({ res }) => sendJson(res, 200, { keys: [jwk] }),
      },
      {
        method: 'GET',
        pattern: '/authorize',
        handler: ({ res, url }) => {
          const params = url.searchParams;
          const problem =
            params.get('response_type') !== 'code'
              ? 'response_type must be code'
              : params.get('client_id') !== options.clientId
                ? 'unknown client_id'
                : !params.get('redirect_uri') || !params.get('state') || !params.get('nonce')
                  ? 'redirect_uri, state and nonce are required'
                  : params.get('code_challenge_method') !== 'S256' || !params.get('code_challenge')
                    ? 'PKCE S256 is required'
                    : !params.get('scope')?.split(' ').includes('openid')
                      ? 'the openid scope is required'
                      : null;
          if (problem) {
            sendText(res, 400, problem);
            return;
          }
          const hidden = [...params]
            .map(([k, v]) => `<input type="hidden" name="${escapeHtml(k)}" value="${escapeHtml(v)}">`)
            .join('');
          sendText(
            res,
            200,
            `<!doctype html><title>Stub ID</title>
<form method="post" action="/authorize">${hidden}
<label>Username <input name="username"></label>
<label>Subject <input name="subject"></label>
<button name="decision" value="approve">Approve</button>
<button name="decision" value="deny">Deny</button>
</form>`,
            'text/html; charset=utf-8',
          );
        },
      },
      {
        method: 'POST',
        pattern: '/authorize',
        handler: ({ res, body }) => {
          const form = new URLSearchParams(typeof body === 'string' ? body : '');
          const redirect = new URL(form.get('redirect_uri') ?? '');
          redirect.searchParams.set('state', form.get('state') ?? '');
          if (form.get('decision') === 'deny') {
            redirect.searchParams.set('error', 'access_denied');
          } else {
            const code = randomUUID();
            codes.set(code, {
              redirectUri: form.get('redirect_uri') ?? '',
              codeChallenge: form.get('code_challenge') ?? '',
              nonce: form.get('nonce') ?? '',
              subject: form.get('subject') ?? '',
              username: form.get('username') ?? '',
            });
            redirect.searchParams.set('code', code);
          }
          res.writeHead(302, { location: redirect.toString() });
          res.end();
        },
      },
      {
        method: 'POST',
        pattern: '/token',
        handler: ({ req, res, body }) => {
          const form = new URLSearchParams(typeof body === 'string' ? body : '');
          const expectedAuth = `Basic ${Buffer.from(
            `${options.clientId}:${options.clientSecret}`,
          ).toString('base64')}`;
          if (req.headers.authorization !== expectedAuth) {
            sendJson(res, 401, { error: 'invalid_client' });
            return;
          }

          const code = form.get('code') ?? '';
          const pending = codes.get(code);
          codes.delete(code); // single use
          const verifier = form.get('code_verifier') ?? '';
          const challenge = createHash('sha256').update(verifier).digest('base64url');
          if (
            form.get('grant_type') !== 'authorization_code' ||
            !pending ||
            pending.redirectUri !== form.get('redirect_uri') ||
            pending.codeChallenge !== challenge
          ) {
            sendJson(res, 400, { error: 'invalid_grant' });
            return;
          }

          const now = Math.floor(Date.now() / 1000);
          sendJson(res, 200, {
            access_token: randomUUID(),
            token_type: 'Bearer',
            expires_in: 300,
            id_token: signIdToken({
              iss: options.issuer,
              aud: options.clientId,
              sub: pending.subject,
              preferred_username: pending.username,
              nonce: pending.nonce,
              iat: now,
              exp: now + 300,
            }),
          });
        },
      },
    ],
  });
}
