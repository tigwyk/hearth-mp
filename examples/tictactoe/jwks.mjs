// JWKS fixture for the JWT provider: one RSA key and one P-256 key, a tiny HTTP
// server that publishes them, and a minting helper. Used by the e2e's JWT phase
// and by the Godot auth smoke test.
//
//   node jwks.mjs serve        # serve http://127.0.0.1:8799/jwks.json (and /count)
//   node jwks.mjs mint-jwt     # print one RS256 token for HEARTH_TEST_JWT
//
// Keys persist in `.jwks-dev.json` (gitignored), so a token minted in one process
// verifies against a JWKS served by another.
import { createHmac, createPrivateKey, generateKeyPairSync, sign } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';

export const HOST = '127.0.0.1';
export const PORT = Number(process.env.JWKS_PORT ?? 8799);
export const ISS = 'https://e2e.hearth.test';
export const AUD = 'hearth-e2e';
/// Persisted next to this script, so minting works from any working directory.
export const KEY_FILE = new URL('./.jwks-dev.json', import.meta.url);
export const RSA_KID = 'rsa-e2e';
export const EC_KID = 'ec-e2e';

/// The public JWKS document served at /jwks.json.
export function jwksDocument(keys) {
  return {
    keys: [
      { ...keys.rsaPublic, kid: RSA_KID, use: 'sig', alg: 'RS256' },
      { ...keys.ecPublic, kid: EC_KID, use: 'sig', alg: 'ES256' },
    ],
  };
}

/// Load the fixture keys, generating and persisting them on first use.
export function ensureKeys() {
  try {
    const saved = JSON.parse(readFileSync(KEY_FILE, 'utf8'));
    return {
      rsaPrivate: createPrivateKey({ key: saved.rsa, format: 'jwk' }),
      ecPrivate: createPrivateKey({ key: saved.ec, format: 'jwk' }),
      rsaPublic: saved.rsaPublic,
      ecPublic: saved.ecPublic,
    };
  } catch {
    const rsa = generateKeyPairSync('rsa', { modulusLength: 2048 });
    const ec = generateKeyPairSync('ec', { namedCurve: 'P-256' });
    const keys = {
      rsa: rsa.privateKey.export({ format: 'jwk' }),
      ec: ec.privateKey.export({ format: 'jwk' }),
      rsaPublic: rsa.publicKey.export({ format: 'jwk' }),
      ecPublic: ec.publicKey.export({ format: 'jwk' }),
    };
    writeFileSync(KEY_FILE, JSON.stringify(keys, null, 2));
    return ensureKeys();
  }
}

function base64url(value) {
  return Buffer.from(value).toString('base64url');
}

/// Mint one compact JWS. `alg` picks the key: RS256 uses the RSA key, ES256 the
/// P-256 key. `overrides` replaces claims, which is how the rejection cases lie.
export function mintJwt(keys, { alg = 'RS256', kid = RSA_KID, sub = 'e2e-player', name = 'E2E Player', overrides = {} } = {}) {
  const header = { alg, kid, typ: 'JWT' };
  const claims = {
    iss: ISS,
    aud: AUD,
    sub,
    exp: Math.floor(Date.now() / 1000) + 300,
    name,
    ...overrides,
  };
  const signingInput = `${base64url(JSON.stringify(header))}.${base64url(JSON.stringify(claims))}`;
  let signature;
  if (alg === 'RS256') {
    signature = sign('sha256', Buffer.from(signingInput), keys.rsaPrivate);
  } else if (alg === 'ES256') {
    // WebCrypto and JWS both want the raw r||s form, not DER.
    signature = sign('sha256', Buffer.from(signingInput), { key: keys.ecPrivate, dsaEncoding: 'ieee-p1363' });
  } else if (alg === 'HS256') {
    // A deliberately wrong algorithm for the JWKS path: HMAC keyed with the RSA
    // public key material, which is exactly the algorithm-confusion attempt.
    signature = createHmac('sha256', JSON.stringify(keys.rsaPublic))
      .update(signingInput)
      .digest();
  } else if (alg === 'none') {
    signature = Buffer.alloc(0);
  } else {
    throw new Error(`unsupported test algorithm ${alg}`);
  }
  return `${signingInput}.${signature.toString('base64url')}`;
}

/// Serve the JWKS on `host:port`; returns `{ server, fetches }` where `fetches`
/// counts requests to /jwks.json so the e2e can prove the cache works.
export function startJwksServer(keys, { port = PORT } = {}) {
  const state = { fetches: 0 };
  const server = createServer((req, res) => {
    if (req.url === '/jwks.json') {
      state.fetches += 1;
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify(jwksDocument(keys)));
      return;
    }
    if (req.url === '/count') {
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ fetches: state.fetches }));
      return;
    }
    res.writeHead(404);
    res.end('not found');
  });
  server.listen(port, HOST);
  return { server, state };
}

const isMain = process.argv[1]?.endsWith('jwks.mjs');
if (isMain) {
  const keys = ensureKeys();
  const [mode = 'serve'] = process.argv.slice(2);
  if (mode === 'mint-jwt') {
    process.stdout.write(`${mintJwt(keys)}\n`);
  } else {
    const { state } = startJwksServer(keys);
    console.log(`jwks: serving http://${HOST}:${PORT}/jwks.json`);
    setInterval(() => {}, 1 << 30);
    process.on('SIGINT', () => {
      console.log(`jwks: ${state.fetches} fetches`);
      process.exit(0);
    });
  }
}
