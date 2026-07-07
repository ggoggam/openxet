// Client-side auth: real OpenXet servers verify OIDC bearer tokens against the
// issuing provider's JWKS. This dev console stores a bearer token you paste
// (obtained from your IdP) and sends it verbatim — there is no shared secret
// and no token minting here. Against a server with auth disabled, leave it blank.

import { useSyncExternalStore } from "react";

const TOKEN_KEY = "openxet.token";

const listeners = new Set<() => void>();

export function getToken(): string {
  return localStorage.getItem(TOKEN_KEY) ?? "";
}

export function setToken(token: string) {
  localStorage.setItem(TOKEN_KEY, token);
  listeners.forEach((l) => l());
}

/** Reactive token: components re-render when it changes anywhere in the app. */
export function useToken(): string {
  return useSyncExternalStore((cb) => {
    listeners.add(cb);
    return () => listeners.delete(cb);
  }, getToken);
}

/** Authorization headers for a /v1 request. Empty when no token is set: a dev
 * server (auth disabled) accepts the request; a server with auth on returns
 * 401 and the error surfaces to the UI. */
export function authHeaders(): HeadersInit {
  const token = getToken();
  return token ? { Authorization: `Bearer ${token}` } : {};
}
