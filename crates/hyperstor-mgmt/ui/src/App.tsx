// The shell: the session (from @glidex/ui-kit), the login page, the
// navigation, and the pages.
import { useState } from "react";
import { NavLink, Navigate, Route, Routes } from "react-router-dom";
import { PamForm, ReauthDialog, SessionProvider, SsoButtons, useSession } from "@glidex/ui-kit";
import { auth, setClaimCode, type Me, type Methods } from "./api";
import { input } from "./ui";
import Overview from "./pages/Overview";
import Nodes from "./pages/Nodes";
import Pools from "./pages/Pools";
import Volumes from "./pages/Volumes";
import Operations from "./pages/Operations";
import Members from "./pages/Members";
import Security from "./pages/Security";

const SSO = [{ label: "your identity provider" }];

function Login({ methods, onLoggedIn }: { methods: Methods; onLoggedIn: () => void }) {
  const [claim, setClaim] = useState("");
  return (
    <div className="max-w-sm mx-auto mt-24 p-6 bg-white dark:bg-slate-900 rounded-xl shadow border border-slate-200 dark:border-slate-700 grid gap-4">
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
  ["/operations", "Operations"],
  ["/members", "Members"],
  ["/security", "Security"],
] as const;

function Shell() {
  const { me, logout } = useSession<Me, Methods>();
  return (
    <div className="min-h-screen">
      <header className="flex flex-wrap items-center gap-4 px-5 py-3 bg-white dark:bg-slate-900 border-b border-slate-200 dark:border-slate-700">
        <span className="font-semibold">Hyperstor</span>
        <nav className="flex flex-wrap gap-1 text-sm">
          {PAGES.map(([to, label]) => (
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
        <span className="ml-auto text-sm text-slate-500" data-testid="whoami">
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
        <Routes>
          <Route path="/" element={<Overview />} />
          <Route path="/nodes" element={<Nodes />} />
          <Route path="/pools" element={<Pools />} />
          <Route path="/volumes" element={<Volumes />} />
          <Route path="/operations" element={<Operations />} />
          <Route path="/members" element={<Members />} />
          <Route path="/security" element={<Security />} />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
      </main>
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
