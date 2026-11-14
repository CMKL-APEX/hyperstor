// The shell: the session (from @glidex/ui-kit), the login page, the
// navigation, and the pages.
import { useEffect, useState } from "react";
import { NavLink, Navigate, Route, Routes } from "react-router-dom";
import { PamForm, ReauthDialog, SessionProvider, SsoButtons, useSession } from "@glidex/ui-kit";
import {
  accounts,
  auth,
  directoryOrigin,
  grantRealms,
  allRealms,
  rolesIn,
  savedRealm,
  setActingRealm,
  setClaimCode,
  setScope,
  ssoProviders,
  tenants,
  type Account,
  type Me,
  type Methods,
} from "./api";
import Footer from "./Footer";
import { ErrorText, input, usePoll, useRealm, useScope } from "./ui";
import Overview from "./pages/Overview";
import Nodes from "./pages/Nodes";
import Pools from "./pages/Pools";
import Volumes from "./pages/Volumes";
import Advisor from "./pages/Advisor";
import Operations from "./pages/Operations";
import Members from "./pages/Members";
import Networks from "./pages/Networks";
import Security from "./pages/Security";
import Trash from "./pages/Trash";
import ObjectStorage from "./pages/ObjectStorage";
import Audit from "./pages/Audit";

/** Where an account comes from, as people know it. */
function accountFrom(a: Account): string {
  return a.directory ? `${a.realm || "directory"} (${directoryOrigin(a.directory) ?? a.directory})` : "this cluster";
}

/** The other accounts this browser holds, to continue as one without
 * signing in again (D310, as glidex D71). */
function HeldAccounts() {
  const held = usePoll(() => accounts.list(), 0);
  const [error, setError] = useState<unknown>();
  const others = (held.data ?? []).filter((a) => !a.active);
  if (others.length === 0) return null;
  return (
    <div className="grid gap-2" data-testid="held-accounts">
      <p className="text-xs text-slate-500">Accounts in this browser</p>
      {others.map((a) => (
        <button
          key={a.user}
          className="text-left px-3 py-2 text-sm rounded-lg border border-slate-300 dark:border-slate-600 hover:bg-slate-50 dark:hover:bg-slate-800"
          onClick={async () => {
            try {
              await accounts.switchTo(a.user);
              window.location.reload();
            } catch (e) {
              setError(e);
            }
          }}
        >
          Continue as {a.user} <span className="text-xs text-slate-500">from {accountFrom(a)}</span>
        </button>
      ))}
      <ErrorText error={error} />
    </div>
  );
}

function Login({ methods, onLoggedIn }: { methods: Methods; onLoggedIn: () => void }) {
  const [claim, setClaim] = useState("");
  return (
    <div className="max-w-sm mx-auto mt-24 p-6 bg-white dark:bg-slate-900 rounded-xl shadow-sm border border-slate-200 dark:border-slate-700 grid gap-4">
      <h1 className="text-lg font-semibold">Hyperstor</h1>
      <HeldAccounts />
      {methods.claim_open && (
        <label className="grid gap-1 text-xs text-slate-500">
          Claim code (the first security administrator; printed by hyperstor-mgmt at start)
          <input
            className={input}
            value={claim}
            autoComplete="off"
            onChange={(e) => {
              setClaim(e.target.value);
              setClaimCode(e.target.value);
            }}
          />
        </label>
      )}
      {methods.pam && <PamForm submitLabel="Log in" onDone={onLoggedIn} />}
      {ssoProviders(methods).length > 0 && <SsoButtons providers={ssoProviders(methods)} />}
      {!methods.pam && ssoProviders(methods).length === 0 && (
        <p className="text-sm text-slate-600">This hyperstor-mgmt has no login method (--authd or --oidc).</p>
      )}
    </div>
  );
}

const PAGES = [
  ["/", "Overview"],
  ["/nodes", "Nodes"],
  ["/pools", "Pools"],
  ["/volumes", "Volumes"],
  ["/s3", "Object storage"],
  ["/advisor", "Advisor"],
  ["/trash", "Trash"],
  ["/operations", "Operations"],
  ["/networks", "Networks"],
  ["/members", "Members"],
  ["/security", "Security"],
  ["/audit", "Audit"],
] as const;

/** What someone acting in a realm sees (D265): its volumes, buckets,
 * keys and placements, its trash, and its people's grants. */
const REALM_PAGES = new Set(["/volumes", "/s3", "/trash", "/operations", "/security"]);

/** The realm the person acts in: someone with roles only in realms picks
 * one of theirs (the first, at first); someone with cluster-wide roles
 * may also pick any realm, or the whole cluster. */
function RealmPicker() {
  const { me } = useSession<Me, Methods>();
  const realm = useRealm();
  const cluster = rolesIn(me.roles, null);
  const reads = cluster.some((r) => ["viewer", "operator", "storage-admin"].includes(r));
  const all = usePoll(async () => (reads ? (await allRealms.listRealms({})).realms.map((r) => r.name) : []), 0);
  const choices = [...new Set([...grantRealms(me.roles), ...(all.data ?? [])])].sort();
  useEffect(() => {
    // The realms there are first, for a choice saved earlier.
    if (reads && all.data === undefined && !all.error) return;
    const ok = (r: string | null) => (r == null ? cluster.length > 0 : choices.includes(r));
    if (ok(realm)) return;
    const saved = savedRealm();
    setActingRealm(ok(saved) ? saved : cluster.length ? null : (choices[0] ?? null));
  });
  if (choices.length === 0) return null;
  return (
    <select
      className={`${input} w-auto`}
      aria-label="Realm"
      value={realm ?? ""}
      onChange={(e) => setActingRealm(e.target.value || null)}
    >
      {cluster.length > 0 && <option value="">Whole cluster</option>}
      {choices.map((r) => (
        <option key={r} value={r}>
          realm {r}
        </option>
      ))}
    </select>
  );
}

/** The header's account menu: who is signed in, the browser's other
 * accounts to switch to or remove, another to add, and signing out of all
 * (D310, as glidex D71). Switching changes the account for every tab. */
function AccountMenu() {
  const { me } = useSession<Me, Methods>();
  const [open, setOpen] = useState(false);
  const [error, setError] = useState<unknown>();
  const held = usePoll(async () => (open ? accounts.list() : []), 0);
  useEffect(() => {
    if (open) held.reload();
  }, [open, held.reload]);
  const act = (f: () => Promise<unknown>, reload = true) => async () => {
    try {
      await f();
      if (reload) window.location.reload();
      else held.reload();
    } catch (e) {
      setError(e);
    }
  };
  const others = (held.data ?? []).filter((a) => !a.active);
  const home = me.directory ? directoryOrigin(me.directory) : null;
  return (
    <div className="relative">
      <button
        className="text-sm px-2 py-1 rounded-md border border-slate-300 dark:border-slate-600"
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen(!open)}
      >
        Accounts
      </button>
      {open && (
        <div
          role="menu"
          className="absolute right-0 mt-1 w-80 z-10 p-3 grid gap-2 bg-white dark:bg-slate-900 border border-slate-200 dark:border-slate-700 rounded-lg shadow-lg text-sm"
        >
          <div>
            Signed in as <b>{me.name}</b>
            {home && (
              <>
                {" "}
                <a className="text-sky-700 dark:text-sky-300 hover:underline" href={`${home}/account`} target="_blank" rel="noreferrer">
                  your directory account
                </a>
              </>
            )}
          </div>
          {others.map((a) => (
            <div key={a.user} className="flex items-center gap-2" data-testid={`account-${a.user}`}>
              <span className="truncate">
                {a.user} <span className="text-xs text-slate-500">from {accountFrom(a)}</span>
              </span>
              <span className="ml-auto" />
              <button role="menuitem" className="text-sky-700 dark:text-sky-300 hover:underline" onClick={act(() => accounts.switchTo(a.user))}>
                Switch
              </button>
              <button role="menuitem" className="text-red-700 dark:text-red-300 hover:underline" onClick={act(() => accounts.remove(a.user), false)}>
                Remove
              </button>
            </div>
          ))}
          <button role="menuitem" className="text-left text-sky-700 dark:text-sky-300 hover:underline" onClick={act(() => accounts.add())}>
            Add another account
          </button>
          <button role="menuitem" className="text-left text-sky-700 dark:text-sky-300 hover:underline" onClick={act(() => accounts.logoutAll())}>
            Sign out of all accounts
          </button>
          <ErrorText error={error ?? held.error} />
        </div>
      )}
    </div>
  );
}

/** The tenant and project the pages show (D310), within the realm acted
 * in, with links to their pages at the glidex directory they come from.
 * Only tenants the person's grants reach are listed (the server's list). */
function ScopePicker() {
  const realm = useRealm();
  const scope = useScope();
  const list = usePoll(async () => (await tenants.listTenants({})).tenants, 0);
  useEffect(() => {
    list.reload();
  }, [realm, list.reload]);
  const all = list.data ?? [];
  const tenant = all.find((t) => t.id === scope.tenant);
  useEffect(() => {
    // A choice from another realm or a tenant no longer reached.
    if (list.data && scope.tenant != null && !tenant) setScope({ tenant: null, project: null });
  });
  if (all.length === 0) return null;
  const project = tenant?.projects.find((p) => p.id === scope.project);
  const origin = tenant?.directory ? directoryOrigin(tenant.directory) : null;
  const link = (href: string, label: string) => (
    <a className="text-xs text-sky-700 dark:text-sky-300 hover:underline" href={href} target="_blank" rel="noreferrer">
      {label}
    </a>
  );
  return (
    <>
      <select
        className={`${input} w-auto`}
        aria-label="Tenant"
        value={scope.tenant ?? ""}
        onChange={(e) => setScope({ tenant: e.target.value ? Number(e.target.value) : null, project: null })}
      >
        <option value="">All tenants</option>
        {all.map((t) => (
          <option key={t.id} value={t.id}>
            tenant {t.name}
          </option>
        ))}
      </select>
      {tenant && (
        <select
          className={`${input} w-auto`}
          aria-label="Project"
          value={scope.project ?? ""}
          onChange={(e) => setScope({ tenant: tenant.id, project: e.target.value ? Number(e.target.value) : null })}
        >
          <option value="">All projects</option>
          {tenant.projects.map((p) => (
            <option key={p.id} value={p.id}>
              project {p.name}
            </option>
          ))}
        </select>
      )}
      {origin && tenant?.glidexId && link(`${origin}/tenants/${tenant.glidexId}`, "tenant in glidex")}
      {origin && project?.glidexId && link(`${origin}/projects/${project.glidexId}`, "project in glidex")}
    </>
  );
}

function Shell() {
  const { me, logout } = useSession<Me, Methods>();
  const realm = useRealm();
  const pages = realm ? PAGES.filter(([to]) => REALM_PAGES.has(to)) : PAGES;
  return (
    <div className="min-h-screen">
      <header className="flex flex-wrap items-center gap-4 px-5 py-3 bg-white dark:bg-slate-900 border-b border-slate-200 dark:border-slate-700">
        <span className="font-semibold">Hyperstor</span>
        <nav className="flex flex-wrap gap-1 text-sm">
          {pages.map(([to, label]) => (
            <NavLink
              key={to}
              to={to}
              end={to === "/"}
              className={({ isActive }) =>
                `px-2 py-1 rounded-md ${isActive ? "bg-sky-100 text-sky-800 dark:bg-sky-900 dark:text-sky-100" : "text-slate-600 dark:text-slate-300 hover:bg-slate-100 dark:hover:bg-slate-800"}`
              }
            >
              {label}
            </NavLink>
          ))}
        </nav>
        <span className="ml-auto" />
        <RealmPicker />
        <ScopePicker />
        <span className="text-sm text-slate-500" data-testid="whoami">
          {me.name} ({me.roles.join(", ") || "no roles yet"})
        </span>
        <AccountMenu />
        <button className="text-sm text-sky-700 dark:text-sky-300 hover:underline" onClick={logout}>
          Log out
        </button>
      </header>
      <main className="max-w-6xl mx-auto p-5 grid gap-4">
        {me.roles.length === 0 && (
          <p className="p-3 rounded-lg bg-amber-50 dark:bg-amber-950 text-amber-900 dark:text-amber-200 text-sm">
            You have no roles yet: a security administrator gives them (Security, People).
          </p>
        )}
        {realm ? (
          <Routes key={realm}>
            <Route path="/volumes" element={<Volumes />} />
            <Route path="/trash" element={<Trash />} />
            <Route path="/s3" element={<ObjectStorage />} />
            <Route path="/operations" element={<Operations />} />
            <Route path="/security" element={<Security />} />
            <Route path="*" element={<Navigate to="/volumes" replace />} />
          </Routes>
        ) : (
        <Routes>
          <Route path="/" element={<Overview />} />
          <Route path="/nodes" element={<Nodes />} />
          <Route path="/pools" element={<Pools />} />
          <Route path="/volumes" element={<Volumes />} />
          <Route path="/advisor" element={<Advisor />} />
          <Route path="/trash" element={<Trash />} />
          <Route path="/s3" element={<ObjectStorage />} />
          <Route path="/operations" element={<Operations />} />
          <Route path="/networks" element={<Networks />} />
          <Route path="/members" element={<Members />} />
          <Route path="/security" element={<Security />} />
          <Route path="/audit" element={<Audit />} />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
        )}
      </main>
      <Footer build={me.build} />
    </div>
  );
}

export default function App() {
  return (
    <SessionProvider<Me, Methods>
      auth={auth}
      login={(methods, onLoggedIn) => <Login methods={methods} onLoggedIn={onLoggedIn} />}
      reauth={({ methods, me, onDone, onCancel }) => (
        <ReauthDialog username={me.name} pam={methods.pam} sso={ssoProviders(methods).length > 0 ? ssoProviders(methods) : null} onDone={onDone} onCancel={onCancel} />
      )}
      loading={<p className="p-6 text-slate-500">Loading...</p>}
      unreachable="Can't reach hyperstor-mgmt"
    >
      <Shell />
    </SessionProvider>
  );
}
