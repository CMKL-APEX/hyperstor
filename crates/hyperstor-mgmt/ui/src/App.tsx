// The shell: the session (from @glidex/ui-kit), the login page, the
// navigation, and the pages.
import { useEffect, useState } from "react";
import { NavLink, Navigate, Route, Routes } from "react-router-dom";
import { PamForm, ReauthDialog, SessionProvider, SsoButtons, useSession } from "@glidex/ui-kit";
import {
  auth,
  grantRealms,
  allRealms,
  rolesIn,
  savedRealm,
  setActingRealm,
  setClaimCode,
  type Me,
  type Methods,
} from "./api";
import Footer from "./Footer";
import { input, usePoll, useRealm } from "./ui";
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

const SSO = [{ label: "your identity provider" }];

function Login({ methods, onLoggedIn }: { methods: Methods; onLoggedIn: () => void }) {
  const [claim, setClaim] = useState("");
  return (
    <div className="max-w-sm mx-auto mt-24 p-6 bg-white dark:bg-slate-900 rounded-xl shadow-sm border border-slate-200 dark:border-slate-700 grid gap-4">
      <h1 className="text-lg font-semibold">Hyperstor</h1>
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
      {methods.oidc && <SsoButtons providers={SSO} />}
      {!methods.pam && !methods.oidc && (
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
        <span className="text-sm text-slate-500" data-testid="whoami">
          {me.name} ({me.roles.join(", ") || "no roles yet"})
        </span>
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
        <ReauthDialog username={me.name} pam={methods.pam} sso={methods.oidc ? SSO : null} onDone={onDone} onCancel={onCancel} />
      )}
      loading={<p className="p-6 text-slate-500">Loading...</p>}
      unreachable="Can't reach hyperstor-mgmt"
    >
      <Shell />
    </SessionProvider>
  );
}
