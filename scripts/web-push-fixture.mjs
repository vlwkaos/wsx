#!/usr/bin/env node
// Local Web Push endpoint for scripts/test-web-client.py.
// It is an independent receiver: it owns the subscription keys, verifies the
// VAPID signature (RFC 8292) and decrypts aes128gcm (RFC 8291) with node:crypto.
// Usage: node web-push-fixture.mjs <output.jsonl>   (prints one JSON line when ready)
import crypto from "node:crypto";
import fs from "node:fs";
import http from "node:http";

const output = process.argv[2];
const b64 = (buffer) => Buffer.from(buffer).toString("base64url");
const receiver = crypto.createECDH("prime256v1");
receiver.generateKeys();
const auth = crypto.randomBytes(16);

function hkdf(salt, ikm, info, length) {
  return Buffer.from(crypto.hkdfSync("sha256", ikm, salt, info, length));
}

function verifyVapid(header, origin) {
  const match = /^vapid t=([\w-]+)\.([\w-]+)\.([\w-]+), k=([\w-]+)$/.exec(header ?? "");
  if (!match) return { ok: false, reason: "malformed Authorization" };
  const [, head, claims, signature, key] = match;
  const point = Buffer.from(key, "base64url");
  const publicKey = crypto.createPublicKey({
    key: { kty: "EC", crv: "P-256", x: b64(point.subarray(1, 33)), y: b64(point.subarray(33, 65)) },
    format: "jwk",
  });
  const ok = crypto.verify(
    "sha256",
    Buffer.from(`${head}.${claims}`),
    { key: publicKey, dsaEncoding: "ieee-p1363" },
    Buffer.from(signature, "base64url"),
  );
  const body = JSON.parse(Buffer.from(claims, "base64url"));
  return {
    ok: ok && body.aud === origin && body.exp > Date.now() / 1000 && body.exp <= Date.now() / 1000 + 24 * 3600,
    key,
    claims: body,
    alg: JSON.parse(Buffer.from(head, "base64url")).alg,
  };
}

function decrypt(body) {
  const salt = body.subarray(0, 16);
  const recordSize = body.readUInt32BE(16);
  const keyLength = body[20];
  const senderPublic = body.subarray(21, 21 + keyLength);
  const ciphertext = body.subarray(21 + keyLength);
  const shared = receiver.computeSecret(senderPublic);
  const info = Buffer.concat([Buffer.from("WebPush: info\0"), receiver.getPublicKey(), senderPublic]);
  const ikm = hkdf(auth, shared, info, 32);
  const key = hkdf(salt, ikm, Buffer.from("Content-Encoding: aes128gcm\0"), 16);
  const nonce = hkdf(salt, ikm, Buffer.from("Content-Encoding: nonce\0"), 12);
  const decipher = crypto.createDecipheriv("aes-128-gcm", key, nonce);
  decipher.setAuthTag(ciphertext.subarray(ciphertext.length - 16));
  const plain = Buffer.concat([decipher.update(ciphertext.subarray(0, ciphertext.length - 16)), decipher.final()]);
  if (plain[plain.length - 1] !== 2) throw new Error("missing last-record delimiter");
  return { recordSize, text: plain.subarray(0, plain.length - 1).toString() };
}

const server = http.createServer((request, response) => {
  const chunks = [];
  request.on("data", (chunk) => chunks.push(chunk));
  request.on("end", () => {
    const record = { path: request.url, method: request.method, encoding: request.headers["content-encoding"], ttl: request.headers.ttl };
    try {
      record.vapid = verifyVapid(request.headers.authorization, `http://127.0.0.1:${server.address().port}`);
      Object.assign(record, decrypt(Buffer.concat(chunks)));
      record.payload = JSON.parse(record.text);
    } catch (error) {
      record.error = String(error);
    }
    fs.appendFileSync(output, JSON.stringify(record) + "\n");
    // "/gone/..." plays a push service that dropped the subscription.
    response.writeHead(request.url.startsWith("/gone/") ? 410 : 201).end();
  });
});

server.listen(0, "127.0.0.1", () => {
  console.log(JSON.stringify({ port: server.address().port, p256dh: b64(receiver.getPublicKey()), auth: b64(auth) }));
});
process.on("SIGTERM", () => server.close(() => process.exit(0)));
