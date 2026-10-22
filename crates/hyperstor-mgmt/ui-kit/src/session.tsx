// The browser session: who is signed in, the login page when nobody is,
// the step-up dialog when a request needs a fresh login. The app gives it
// an AuthApi and, optionally, what else to load once signed in (`load`).
import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from "react";
import { AuthApiProvider, type AuthApi, type WhoAmIBase } from "./auth";
import { isHttpError, setCsrf, setSessionHandlers } from "./client";

export interface Session<W extends WhoAmIBase = WhoAmIBase, M = unknown, X = undefined> {
  me: W;
  methods: M;
  /** What `load` gave. */
  extra: X;
  /** Change `extra` in place (say, after re-running a check). */
  setExtra: (update: (x: X) => X) => void;
  /** whoami and `load` again. */
  refresh: () => Promise<void>;
  logout: () => Promise<void>;
}

const SessionContext = createContext<Session<WhoAmIBase, unknown, unknown> | null>(null);

/** The session; only under SessionProvider, which renders its children
 * only once signed in. */
export function useSession<W extends WhoAmIBase = WhoAmIBase, M = unknown, X = undefined>(): Session<W, M, X> {
  const s = useContext(SessionContext);
  if (!s) throw new Error("useSession outside SessionProvider");
  return s as unknown as Session<W, M, X>;
}

type State<W, M, X> =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | { kind: "anonymous"; methods: M }
  | { kind: "ready"; me: W; methods: M; extra: X };

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

export interface SessionProviderProps<W extends WhoAmIBase, M, X> {
  auth: AuthApi<W, M>;
  /** What else to load once signed in; a throw shows the error panel. */
  load?: (me: W) => Promise<X>;
  /** The login page, for nobody signed in. */
  login: (methods: M, onLoggedIn: () => void) => ReactNode;
  /** The step-up dialog (ReauthDialog, say). */
  reauth: (p: { methods: M; me: W; onDone: () => void; onCancel: () => void }) => ReactNode;
  /** Shown while loading. */
  loading?: ReactNode;
  /** The error panel's heading when the methods or whoami fail. */
  unreachable?: string;
  children: ReactNode;
}

export function SessionProvider<W extends WhoAmIBase, M, X = undefined>({
  auth,
  load: loadExtra,
  login,
  reauth: renderReauth,
  loading = null,
  unreachable = "Can't reach the server",
  children,
}: SessionProviderProps<W, M, X>) {
  const [state, setState] = useState<State<W, M, X>>({ kind: "loading" });
  const [reauth, setReauth] = useState<((ok: boolean) => void) | null>(null);

  const load = useCallback(async () => {
    let methods: M;
    try {
      methods = await auth.methods();
    } catch (e) {
      setState({ kind: "error", message: message(e) });
      return;
    }
    let me: W;
    try {
      me = await auth.whoami();
    } catch (e) {
      if (isHttpError(e) && e.status === 401) {
        setState({ kind: "anonymous", methods });
      } else {
        setState({ kind: "error", message: message(e) });
      }
      return;
    }
    if (me.csrf !== undefined) setCsrf(me.csrf);
    let extra: X;
    try {
      extra = loadExtra ? await loadExtra(me) : (undefined as X);
    } catch (e) {
      setState({ kind: "error", message: message(e) });
      return;
    }
    setState({ kind: "ready", me, methods, extra });
  }, [auth, loadExtra]);

  useEffect(() => {
    setSessionHandlers({
      unauthenticated: () => {
        setCsrf(null);
        setState((s) => (s.kind === "ready" ? { kind: "anonymous", methods: s.methods } : s));
      },
      reauth: () => new Promise<boolean>((resolve) => setReauth(() => resolve)),
    });
    load();
  }, [load]);

  const finishReauth = (ok: boolean) => {
    reauth?.(ok);
    setReauth(null);
  };

  const setExtra = useCallback((update: (x: X) => X) => {
    setState((s) => (s.kind === "ready" ? { ...s, extra: update(s.extra) } : s));
  }, []);

  const logout = useCallback(async () => {
    await auth.logout().catch(() => {});
    setState((s) => (s.kind === "ready" ? { kind: "anonymous", methods: s.methods } : s));
  }, [auth]);

  const session = useMemo<Session<W, M, X> | null>(() => {
    if (state.kind !== "ready") return null;
    return { me: state.me, methods: state.methods, extra: state.extra, setExtra, refresh: load, logout };
  }, [state, setExtra, load, logout]);

  let body: ReactNode;
  switch (state.kind) {
    case "loading":
      body = loading;
      break;
    case "error":
      body = (
        <div className="max-w-lg mx-auto mt-24 p-6 bg-white rounded-xl shadow-md border border-red-200">
          <h1 className="text-lg font-semibold text-red-700">{unreachable}</h1>
          <p className="mt-2 text-sm text-gray-600">{state.message}</p>
          <button
            className="mt-4 px-4 py-2 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg"
            onClick={() => {
              setState({ kind: "loading" });
              load();
            }}
          >
            Retry
          </button>
        </div>
      );
      break;
    case "anonymous":
      body = login(state.methods, load);
      break;
    case "ready":
      body = <SessionContext.Provider value={session as unknown as Session<WhoAmIBase, unknown, unknown>}>{children}</SessionContext.Provider>;
      break;
  }
  return (
    <AuthApiProvider api={auth}>
      {body}
      {reauth &&
        state.kind === "ready" &&
        renderReauth({ methods: state.methods, me: state.me, onDone: () => finishReauth(true), onCancel: () => finishReauth(false) })}
    </AuthApiProvider>
  );
}
