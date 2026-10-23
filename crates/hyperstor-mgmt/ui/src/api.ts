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

const transport = createConnectTransport({
  baseUrl: "/api",
  useBinaryFormat: false,
  interceptors: [csrf],
  fetch: (input, init) => fetch(input, { ...init, credentials: "same-origin" }),
});

/** A client whose every call goes through the session's 401 handling. */
function client<S extends DescService>(service: S): Client<S> {
  const raw = createClient(service, transport) as Record<string, (...a: unknown[]) => Promise<unknown>>;
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

/** mgmt's own JSON routes. */
async function call<T>(method: string, path: string, body?: unknown, quiet401 = false): Promise<T> {
  return withSession(
    async () => {
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
}

/** Who is logged in (whoami, shaped for the kit). */
export interface Me extends WhoAmIBase {
  name: string;
  roles: string[];
  method: string;
}

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
    const w = await call<{ user: string; roles: string[]; method: string; csrf: string | null }>(
      "GET",
      "/auth/whoami",
      undefined,
      true,
    );
    return { name: w.user, roles: w.roles, method: w.method, csrf: w.csrf, user: { display_name: w.user } };
  },
  login: async (user, password) => {
    const body: Record<string, string> = { user, password };
    if (claimCode) body.claim = claimCode;
    const r = await call<{ csrf: string }>("POST", "/auth/login", body, true);
    setCsrf(r.csrf);
    claimCode = "";
    return r;
  },
  startSso: (_provider, returnTo, reauth) => {
    const body: Record<string, unknown> = { return_to: returnTo, reauth };
    if (claimCode) body.claim = claimCode;
    call<{ redirect: string }>("POST", "/auth/oidc/start", body, true)
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
