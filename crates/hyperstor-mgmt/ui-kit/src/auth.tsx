import { createContext, useContext, type ReactNode } from "react";

/** What every signed-in session knows; the app's whoami adds its own fields. */
export interface WhoAmIBase {
  /** A browser session's CSRF value. */
  csrf?: string | null;
  user?: { display_name?: string } | null;
}

/** The app's login API, as the kit's components need it. `M` is the app's
 * login methods (what its login page offers), `W` its whoami. Errors
 * should carry `status` and `code` (HttpError): a 401 from whoami means
 * "not signed in", `login_failed` from login "wrong password". */
export interface AuthApi<W extends WhoAmIBase = WhoAmIBase, M = unknown> {
  /** What the login page offers. */
  methods(): Promise<M>;
  /** Who is signed in; throws a 401 when nobody is. */
  whoami(): Promise<W>;
  /** A username and password login. */
  login(username: string, password: string): Promise<unknown>;
  /** Leave for single sign-on with `provider` (undefined: the default one),
   * coming back to `returnTo`; `reauth` for a step-up. */
  startSso(provider: string | undefined, returnTo: string, reauth: boolean): void;
  logout(): Promise<void>;
}

const AuthApiContext = createContext<AuthApi | null>(null);

/** Gives PamForm and SsoButtons the app's AuthApi (SessionProvider does
 * this for everything under it). */
export function AuthApiProvider<W extends WhoAmIBase, M>({ api, children }: { api: AuthApi<W, M>; children: ReactNode }) {
  return <AuthApiContext.Provider value={api as unknown as AuthApi}>{children}</AuthApiContext.Provider>;
}

export function useAuthApi(): AuthApi {
  const a = useContext(AuthApiContext);
  if (!a) throw new Error("useAuthApi outside AuthApiProvider");
  return a;
}
