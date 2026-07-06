// Client-side auth: the server validates HS256 JWTs signed with its
// OPENXET_AUTH_SECRET (claims: scope/repo/exp). There is no token-issuing
// endpoint — on huggingface.co that's the Hub's job — so this dev console
// mints tokens in the browser via WebCrypto from a user-provided secret.

import { useSyncExternalStore } from "react";

const SECRET_KEY = "openxet.secret";

const listeners = new Set<() => void>();

export function getSecret(): string {
  return localStorage.getItem(SECRET_KEY) ?? "";
}

export function setSecret(secret: string) {
  localStorage.setItem(SECRET_KEY, secret);
  listeners.forEach((l) => l());
}

/** Reactive secret: components re-render when it changes anywhere in the app. */
export function useSecret(): string {
  return useSyncExternalStore((cb) => {
    listeners.add(cb);
    return () => listeners.delete(cb);
  }, getSecret);
}

function b64url(bytes: Uint8Array): string {
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

export type Scope = "read" | "write";

export async function mintToken(scope: Scope): Promise<string> {
  const secret = getSecret();
  if (!secret) {
    throw new Error(
      "No auth secret configured — paste the server's OPENXET_AUTH_SECRET in the header field.",
    );
  }
  const enc = new TextEncoder();
  const header = b64url(enc.encode(JSON.stringify({ alg: "HS256", typ: "JWT" })));
  const payload = b64url(
    enc.encode(
      JSON.stringify({
        scope,
        repo: "web/ui",
        exp: Math.floor(Date.now() / 1000) + 3600,
      }),
    ),
  );
  const key = await crypto.subtle.importKey(
    "raw",
    enc.encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const sig = new Uint8Array(
    await crypto.subtle.sign("HMAC", key, enc.encode(`${header}.${payload}`)),
  );
  return `${header}.${payload}.${b64url(sig)}`;
}
