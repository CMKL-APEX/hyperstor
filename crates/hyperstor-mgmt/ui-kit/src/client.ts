// The browser session's plumbing, shared by the app's API client and the
// kit's SessionProvider: the CSRF value, and the two 401 handlers. One
// session per page, so module state.

/** An error from the app's API: its HTTP status and machine-readable code
 * (`reauth_required`, `unauthenticated`, `login_failed`, ...). */
export interface HttpError {
  status: number;
  code: string;
}

export function isHttpError(e: unknown): e is HttpError {
  return typeof e === "object" && e !== null && typeof (e as HttpError).status === "number" && typeof (e as HttpError).code === "string";
}

/** The session's CSRF value, from whoami or a login response. Kept in
 * memory only: a fresh page load learns it from whoami. */
let csrf: string | null = null;

export function setCsrf(value: string | null | undefined) {
  csrf = value ?? null;
}

/** The value to send on writes, or null before whoami. */
export function csrfToken(): string | null {
  return csrf;
}

/** Called when the session is gone (401 unauthenticated). */
let onUnauthenticated: () => void = () => {};
/** Asks the user to log in again (401 reauth_required); resolves true when they did. */
let onReauth: () => Promise<boolean> = async () => false;

/** Set by SessionProvider; an app without it gets no-ops. */
export function setSessionHandlers(h: { unauthenticated: () => void; reauth: () => Promise<boolean> }) {
  onUnauthenticated = h.unauthenticated;
  onReauth = h.reauth;
}

/** Tell the session it ended (a 401 `unauthenticated`): back to the login page. */
export function sessionEnded() {
  onUnauthenticated();
}

/** Ask the user to log in again (a 401 `reauth_required`); true when they did. */
export function reauthenticate(): Promise<boolean> {
  return onReauth();
}

/** Run `send`, with the 401 handling: a step-up (`reauth_required`) asks
 * the user to log in again, then sends once more; `unauthenticated` ends
 * the session unless `quiet401` (whoami, login). `toError` turns what
 * `send` throws into the app's error (one with `status` and `code`). */
export async function withSession<T>(
  send: () => Promise<T>,
  opts: { quiet401?: boolean; toError?: (e: unknown) => unknown } = {},
): Promise<T> {
  const toError = opts.toError ?? ((e: unknown) => e);
  try {
    return await send();
  } catch (e) {
    let err = toError(e);
    if (isHttpError(err) && err.status === 401) {
      if (err.code === "reauth_required" && (await reauthenticate())) {
        try {
          return await send();
        } catch (e2) {
          err = toError(e2);
        }
      }
      if (isHttpError(err) && err.code === "unauthenticated" && !opts.quiet401) sessionEnded();
    }
    throw err;
  }
}
