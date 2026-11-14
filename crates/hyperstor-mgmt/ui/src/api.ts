// The UI's way to hyperstor-mgmt: the cluster API through mgmt (Connect,
// JSON, under /api), and mgmt's own /auth routes. Every call goes with the
// session cookie; writes carry its CSRF value. A refusal becomes an
// HttpError (status and reason) for @glidex/ui-kit's session handling:
// `reauth_required` asks for a fresh login and sends once more,
// `unauthenticated` returns to the login page.
import { Code, ConnectError, createClient, type Client, type Interceptor } from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-web";
import type { DescService } from "@bufbuild/protobuf";
import { csrfToken, setCsrf, withSession, type AuthApi, type HttpError, type WhoAmIBase } from "@glidex/ui-kit";
import { ClusterService, TrashService } from "./gen/hyperstor/v1/cluster_pb";
import {
  AuditService,
  CaService,
  ErrorInfoSchema,
  MemberService,
  NodeService,
  OperationService,
  RealmService,
  S3Service,
  TenantService,
  TokenService,
} from "./gen/hyperstor/v1/admin_pb";

/** A refusal: the HTTP status, the reason (`reauth_required`,
 * `permission_denied`, `in_use`...) and the message. */
export class ApiError extends Error implements HttpError {
  constructor(
    public status: number,
    public code: string,
    message: string,
  ) {
    super(message);
  }
}

const HTTP_OF: Partial<Record<Code, number>> = {
  [Code.InvalidArgument]: 400,
  [Code.Unauthenticated]: 401,
  [Code.PermissionDenied]: 403,
  [Code.NotFound]: 404,
  [Code.AlreadyExists]: 409,
  [Code.FailedPrecondition]: 412,
  [Code.ResourceExhausted]: 429,
  [Code.Unavailable]: 503,
};

/** What the API refused, as an ApiError: its reason from the ErrorInfo
 * detail, as hyperstorctl shows it. */
export function toApiError(e: unknown): unknown {
  if (!(e instanceof ConnectError)) return e;
  const info = e.findDetails(ErrorInfoSchema)[0];
  const status = HTTP_OF[e.code] ?? 500;
  const reason = info?.reason || Code[e.code].replace(/([a-z])([A-Z])/g, "$1_$2").toLowerCase();
  return new ApiError(status, reason, e.rawMessage);
}

const csrf: Interceptor = (next) => async (req) => {
  const t = csrfToken();
  if (t) req.header.set("X-Hyperstor-CSRF", t);
  return next(req);
};

/** The realm the person acts in (D265), or null for the cluster as a
 * whole: every call says so, and mgmt then reaches that realm alone. */
let actingRealm: string | null = null;
const realmListeners = new Set<() => void>();
export function getActingRealm(): string | null {
  return actingRealm;
}
let realmChosen = false;
export function setActingRealm(r: string | null) {
  // Another realm's tenants: the tenant and project picked go (not at the
  // first choice, which restores the last one's).
  if (realmChosen && r !== actingRealm) setScope({ tenant: null, project: null });
  realmChosen = true;
  actingRealm = r;
  try {
    if (r) localStorage.setItem("hyperstor.realm", r);
    else localStorage.removeItem("hyperstor.realm");
  } catch {
    // No storage: the choice lasts this page.
  }
  realmListeners.forEach((f) => f());
}
export function onActingRealm(f: () => void): () => void {
  realmListeners.add(f);
  return () => realmListeners.delete(f);
}
/** The realm chosen last time on this browser, if any. */
export function savedRealm(): string | null {
  try {
    return localStorage.getItem("hyperstor.realm");
  } catch {
    return null;
  }
}

/** The tenant and project the pages show (D310): a view within what the
 * person's grants reach, chosen in the header; null for all. Creations
 * go to the chosen project. The server's reach doesn't change with it
 * (I400). */
export interface Scope {
  tenant: number | null;
  project: number | null;
}
let scope: Scope = { tenant: savedId("hyperstor.tenant"), project: savedId("hyperstor.project") };
const scopeListeners = new Set<() => void>();
function savedId(k: string): number | null {
  try {
    const v = Number(localStorage.getItem(k) ?? "");
    return Number.isInteger(v) && v > 0 ? v : null;
  } catch {
    return null;
  }
}
export function getScope(): Scope {
  return scope;
}
export function setScope(s: Scope) {
  scope = s;
  try {
    for (const [k, v] of [
      ["hyperstor.tenant", s.tenant],
      ["hyperstor.project", s.project],
    ] as const) {
      if (v) localStorage.setItem(k, String(v));
      else localStorage.removeItem(k);
    }
  } catch {
    // No storage: the choice lasts this page.
  }
  scopeListeners.forEach((f) => f());
}
export function onScope(f: () => void): () => void {
  scopeListeners.add(f);
  return () => scopeListeners.delete(f);
}
/** Whether a volume, bucket or key of `tenant` in `project` (0: the
 * tenant's default project, whose id is `defaultProject`) is shown. */
export function inScope(s: Scope, tenant: number, project: number, defaultProject = 0): boolean {
  if (s.tenant != null && tenant !== s.tenant) return false;
  if (s.project != null && (project || defaultProject) !== s.project) return false;
  return true;
}

const realmHeader: Interceptor = (next) => async (req) => {
  if (actingRealm) req.header.set("X-Hyperstor-Realm", actingRealm);
  return next(req);
};

const transport = createConnectTransport({
  baseUrl: "/api",
  useBinaryFormat: false,
  interceptors: [csrf, realmHeader],
  fetch: (input, init) => fetch(input, { ...init, credentials: "same-origin" }),
});

/** For the realm picker: the realms there are, whichever is chosen. */
const anyRealm = createConnectTransport({
  baseUrl: "/api",
  useBinaryFormat: false,
  interceptors: [csrf],
  fetch: (input, init) => fetch(input, { ...init, credentials: "same-origin" }),
});

/** A client whose every call goes through the session's 401 handling. */
function client<S extends DescService>(service: S, via = transport): Client<S> {
  const raw = createClient(service, via) as Record<string, (...a: unknown[]) => Promise<unknown>>;
  return new Proxy(raw, {
    get(target, prop: string) {
      const f = target[prop];
      if (typeof f !== "function") return f;
      return (...args: unknown[]) => withSession(() => f(...args), { toError: toApiError });
    },
  }) as unknown as Client<S>;
}

export const cluster = client(ClusterService);
export const tokens = client(TokenService);
export const ca = client(CaService);
export const members = client(MemberService);
export const nodes = client(NodeService);
export const ops = client(OperationService);
export const trash = client(TrashService);
export const audit = client(AuditService);
export const s3 = client(S3Service);
export const realms = client(RealmService);
export const tenants = client(TenantService);
/** Every realm the person may see, not only the one they act in. */
export const allRealms = client(RealmService, anyRealm);

/** mgmt's own JSON routes. */
async function call<T>(method: string, path: string, body?: unknown, quiet401 = false): Promise<T> {
  return withSession(
    async () => {
      const headers: Record<string, string> = {};
      if (body !== undefined) headers["Content-Type"] = "application/json";
      const t = csrfToken();
      if (method !== "GET" && t) headers["X-Hyperstor-CSRF"] = t;
      if (actingRealm) headers["X-Hyperstor-Realm"] = actingRealm;
      const r = await fetch(path, {
        method,
        headers,
        body: body === undefined ? undefined : JSON.stringify(body),
        credentials: "same-origin",
      });
      const j = await r.json().catch(() => ({}));
      if (!r.ok) throw new ApiError(r.status, j.reason ?? j.code ?? "error", j.message ?? `${path}: ${r.status}`);
      return j as T;
    },
    { quiet401 },
  );
}

/** What the login page offers. */
export interface Methods {
  pam: boolean;
  oidc: boolean;
  claim_open: boolean;
  /** Trusted glidex directories people sign in through (D307). */
  directories?: { id: string; issuer: string; realm: string }[];
}

/** The sign-in choices besides a password: the identity provider, and
 * each trusted directory (its provider id `dir:<id>`). */
export function ssoProviders(m: Methods): { id?: string; label: string }[] {
  const out: { id?: string; label: string }[] = m.oidc ? [{ label: "your identity provider" }] : [];
  for (const d of m.directories ?? []) out.push({ id: `dir:${d.id}`, label: d.realm });
  return out;
}

/** The build of the mgmt serving the UI (its `build.rs`). */
export interface Build {
  /** The version tag on the built commit. */
  tag: string | null;
  /** That tag, else the latest release tag before the commit. */
  release: string | null;
  /** Commits after `release` (0 on a tagged build). */
  ahead: number;
  branch: string | null;
  commit: string | null;
  /** The repository the footer links to, `https://github.com/<owner>/<repo>`. */
  repo: string;
}

/** Who is logged in (whoami, shaped for the kit). */
export interface Me extends WhoAmIBase {
  name: string;
  roles: string[];
  method: string;
  build: Build | null;
  /** The issuer of the glidex directory they signed in through, if any. */
  directory: string | null;
}

/** The web origin of a directory's issuer, where its pages are (D310). */
export function directoryOrigin(issuer: string): string | null {
  try {
    return new URL(issuer).origin;
  } catch {
    return null;
  }
}

/** An account this browser holds (D310, as glidex D71). */
export interface Account {
  user: string;
  /** Its directory's issuer; empty for a local account. */
  directory: string;
  realm: string;
  method: string;
  active: boolean;
}

/** The browser's accounts, outside the session's handling: they work on
 * the login page too. */
async function plain<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = {};
  if (body !== undefined) headers["Content-Type"] = "application/json";
  const t = csrfToken();
  if (method !== "GET" && t) headers["X-Hyperstor-CSRF"] = t;
  const r = await fetch(path, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
    credentials: "same-origin",
  });
  const j = await r.json().catch(() => ({}));
  if (!r.ok) throw new ApiError(r.status, j.reason ?? "error", j.message ?? `${path}: ${r.status}`);
  return j as T;
}

export const accounts = {
  list: () => plain<{ accounts: Account[] }>("GET", "/auth/accounts").then((r) => r.accounts),
  switchTo: async (user: string) => {
    const r = await plain<{ csrf: string | null }>("POST", "/auth/accounts/switch", { user });
    setCsrf(r.csrf);
  },
  add: () => plain("POST", "/auth/accounts/add", {}),
  remove: (user: string) => plain("POST", "/auth/accounts/remove", { user }),
  logoutAll: () => plain("POST", "/auth/accounts/logout-all", {}),
};

export interface Person {
  name: string;
  roles: string[];
  created: number;
  disabled: boolean;
}

export interface ApiToken {
  id: string;
  name: string;
  user: string;
  created: number;
  expires: number;
}

export const ROLES = ["viewer", "operator", "storage-admin", "security-admin"] as const;

/** A grant's role and realm: `viewer` is cluster-wide, `viewer@acme` is
 * in realm acme (D263). */
export function grantOf(g: string): [string, string | null] {
  const i = g.indexOf("@");
  return i < 0 ? [g, null] : [g.slice(0, i), g.slice(i + 1)];
}

/** The realms a person holds grants in, by name. */
export function grantRealms(roles: string[]): string[] {
  return [...new Set(roles.map((g) => grantOf(g)[1]).filter((r): r is string => r != null))].sort();
}

/** The roles in effect acting in `realm` (null: the cluster as a whole):
 * the cluster-wide ones, and that realm's. */
export function rolesIn(roles: string[], realm: string | null): string[] {
  return roles
    .map(grantOf)
    .filter(([, r]) => r == null || r === realm)
    .map(([role]) => role);
}

/** The first security administrator's claim code, typed on the login
 * page: it goes with the next login, in the body, never in a URL. */
let claimCode = "";
export function setClaimCode(c: string) {
  claimCode = c.trim();
}

/** The login API, as @glidex/ui-kit's SessionProvider needs it. */
export const auth: AuthApi<Me, Methods> = {
  methods: () => call<Methods>("GET", "/auth/methods"),
  whoami: async () => {
    const w = await call<{
      user: string;
      roles: string[];
      method: string;
      csrf: string | null;
      build?: Build;
      directory?: string | null;
    }>(
      "GET",
      "/auth/whoami",
      undefined,
      true,
    );
    return {
      name: w.user,
      roles: w.roles,
      method: w.method,
      csrf: w.csrf,
      build: w.build ?? null,
      directory: w.directory ?? null,
      user: { display_name: w.user },
    };
  },
  login: async (user, password) => {
    const body: Record<string, string> = { user, password };
    if (claimCode) body.claim = claimCode;
    const r = await call<{ csrf: string }>("POST", "/auth/login", body, true);
    setCsrf(r.csrf);
    claimCode = "";
    return r;
  },
  startSso: (provider, returnTo, reauth) => {
    const body: Record<string, unknown> = { return_to: returnTo, reauth };
    let path = "/auth/oidc/start";
    if (provider?.startsWith("dir:")) {
      // A glidex directory (D307): its own login, passkeys and MFA.
      body.directory = provider.slice(4);
      path = "/auth/directory/start";
    } else if (claimCode) body.claim = claimCode;
    call<{ redirect: string }>("POST", path, body, true)
      .then((r) => window.location.assign(r.redirect))
      .catch((e) => alert(e instanceof Error ? e.message : String(e)));
  },
  logout: async () => {
    await call("POST", "/auth/logout", undefined, true);
    setCsrf(null);
  },
};

export const people = {
  list: () => call<{ users: Person[] }>("GET", "/auth/users").then((r) => r.users),
  setRoles: (name: string, roles: string[]) =>
    call<{ user: Person }>("PUT", `/auth/users/${encodeURIComponent(name)}/roles`, { roles }),
  remove: (name: string) => call("DELETE", `/auth/users/${encodeURIComponent(name)}`),
  tokens: () => call<{ tokens: ApiToken[] }>("GET", "/auth/tokens").then((r) => r.tokens),
  createToken: (name: string, days: number) =>
    call<{ token: string; info: ApiToken }>("POST", "/auth/tokens", { name, days }),
  revokeToken: (id: string) => call("DELETE", `/auth/tokens/${encodeURIComponent(id)}`),
};

/** One node's metrics, as mgmt parses them. */
export interface NodeMetrics {
  addr: string;
  up: boolean;
  samples: [string, Record<string, string>, number][];
}

export const metrics = () => call<{ nodes: NodeMetrics[] }>("GET", "/api/metrics").then((r) => r.nodes);
