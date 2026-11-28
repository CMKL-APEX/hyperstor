// Tenancy (20-tenants.md §24.8, §24.9; 19-administration.md §23.12): what
// hyperstorctl's realm, tenant, project and admission commands manage, on
// one page of the cluster's (not a realm's) in three tabs.
//
// - Realms: each realm's pools, hosts and management clients; create and
//   delete realms; bind pools (storage administrators), hosts and clients
//   (security administrators, D251, D311, D327).
// - Tenants & projects: realm, tenant, project as a tree with the space
//   each uses (GetSpace, D335); create, rename, delete, a tenant's realm
//   (security administrators, D250), and a project's move to another
//   tenant, a long operation (D303). What a glidex directory owns (names
//   and membership of linked records) is read-only here (I413).
// - Directories (security administrators): the trusted directories, their
//   feed's age and staleness limit (D324), the operator tenant, ending a
//   trust; a directory trusted as a whole, its admission setting and
//   default caps, and its tenants' opt-ins to approve or refuse (D345–D347);
//   offers to a tenant or to a whole directory, whose token is shown once
//   and never kept (D306).
import { useEffect, useState, type ReactNode } from "react";
import { Link } from "react-router-dom";
import { Modal, TokenReveal } from "@glidex/ui-kit";
import { cluster, directoryOrigin, offerPin, ops, realms, tenants, trust } from "../api";
import { OperationState, type Project, type Realm, type Tenant, type TrustedDirectory, type TrustOffer } from "../gen/hyperstor/v1/admin_pb";
import type { SpaceRow } from "../gen/hyperstor/v1/cluster_pb";
import { ActionDialog, Button, Card, Cli, ErrorText, Field, Pill, Table, fmtBytes, fmtTime, input, parseSize, td, useCan, usePoll } from "../ui";

/** A realm list as hyperstorctl takes it: names, or `any` for none. */
const realmArg = (rs: string[]) => (rs.length ? rs.join(",") : "any");

/** A directory's short name: the last segment of its issuer
 * (`https://host/d/NAME`), as its realm is named by default (D318). */
function directoryName(issuer: string): string {
  try {
    const parts = new URL(issuer).pathname.split("/").filter(Boolean);
    return parts[parts.length - 1] ?? issuer;
  } catch {
    return issuer;
  }
}

/** Checkboxes for a set of realms; none ticked means `none` (the caller
 * says what that is: every realm, or unbound). */
function RealmBoxes({ all, picked, onChange, none }: { all: string[]; picked: string[]; onChange: (r: string[]) => void; none: string }) {
  return (
    <fieldset className="grid gap-1">
      <legend className="text-xs text-slate-500 mb-1">Realms (none ticked: {none})</legend>
      {all.map((r) => (
        <label key={r} className="text-sm flex gap-2 items-center">
          <input
            type="checkbox"
            aria-label={`realm ${r}`}
            checked={picked.includes(r)}
            onChange={(e) => onChange(e.target.checked ? [...picked, r] : picked.filter((x) => x !== r))}
          />
          {r}
        </label>
      ))}
    </fieldset>
  );
}

/** A delete confirmed by typing the name of what goes. */
function TypedDelete({
  title,
  name,
  cli,
  run,
  onClose,
  children,
}: {
  title: string;
  name: string;
  cli: string;
  run: () => Promise<unknown>;
  onClose: (done: boolean) => void;
  children?: ReactNode;
}) {
  const [typed, setTyped] = useState("");
  return (
    <ActionDialog title={title} submit="Delete" kind="danger" cli={cli} run={run} onClose={onClose} disabled={typed !== name}>
      {children}
      <Field label={`Type ${name} to confirm`}>
        <input className={input} aria-label="Confirm name" value={typed} autoComplete="off" onChange={(e) => setTyped(e.target.value)} />
      </Field>
      <p className="text-xs text-slate-500">This needs a login of the last 10 minutes.</p>
    </ActionDialog>
  );
}

// ---------------------------------------------------------------- Realms

type RealmDialog =
  | { kind: "create" }
  | { kind: "delete"; realm: Realm }
  | { kind: "pool"; pool: number; name: string; realms: string[] }
  | { kind: "host"; host: string; realms: string[]; isNew: boolean }
  | { kind: "client"; client: string; realms: string[]; isNew: boolean }
  | { kind: "clientTokens"; client: string; allow: boolean };

function RealmsTab() {
  const can = useCan();
  const list = usePoll(() => realms.listRealms({}), 5000);
  const [dialog, setDialog] = useState<RealmDialog | null>(null);
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [picked, setPicked] = useState<string[]>([]);
  const all = list.data?.realms ?? [];
  const names = all.map((r) => r.name);
  // Every pool, from the realms' lists and those open to every realm:
  // ListRealms alone, which security administrators read too (I413).
  const poolNames = new Map<number, string>();
  for (const r of all) r.poolIds.forEach((id, i) => poolNames.set(id, r.pools[i] ?? ""));
  list.data?.openPoolIds.forEach((id, i) => poolNames.set(id, list.data?.openPools[i] ?? ""));
  const pools = [...poolNames.entries()]
    .filter(([id]) => id !== 0)
    .sort(([a], [b]) => a - b)
    .map(([id, name]) => ({ id, name }));
  // Each host's and pool's realms, from the realms that name them.
  const hosts = new Map<string, string[]>();
  for (const r of all) for (const h of r.hosts) hosts.set(h, [...(hosts.get(h) ?? []), r.name]);
  const poolRealms = (id: number) => all.filter((r) => r.poolIds.includes(id)).map((r) => r.name);
  const clients = list.data?.clients ?? [];
  const close = (done: boolean) => {
    setDialog(null);
    if (done) list.reload();
  };
  const open = (d: RealmDialog) => {
    setName("");
    setDescription("");
    if (d.kind === "pool" || d.kind === "host" || d.kind === "client") setPicked(d.realms);
    setDialog(d);
  };
  return (
    <>
      <Card title="Realms" actions={can("store") && <Button onClick={() => open({ kind: "create" })}>Create realm</Button>}>
        <ErrorText error={list.error} />
        <Table head={["Realm", "Description", "Tenants", "Pools", "Hosts", "Clients", ""]}>
          {all.map((r) => (
            <tr key={r.name} data-testid={`tenancy-realm-${r.name}`}>
              <td className={td}>{r.name}</td>
              <td className={`${td} whitespace-normal`}>{r.description || "–"}</td>
              <td className={td}>{r.name === "default" ? "the others" : r.tenants.length}</td>
              <td className={`${td} whitespace-normal`}>{r.pools.join(", ") || "–"}</td>
              <td className={`${td} whitespace-normal`}>{r.hosts.join(", ") || "–"}</td>
              <td className={`${td} whitespace-normal`}>{r.clients.join(", ") || "–"}</td>
              <td className={td}>
                {r.name === "default" ? (
                  <span className="text-xs text-slate-500">always there; can&apos;t be deleted</span>
                ) : (
                  can("store") && (
                    <Button kind="danger" onClick={() => open({ kind: "delete", realm: r })}>
                      Delete
                    </Button>
                  )
                )}
              </td>
            </tr>
          ))}
        </Table>
        <p className="text-xs text-slate-500 mt-2">
          A realm groups tenants with their own pools, S3 gateway and keys. &ldquo;default&rdquo; holds every tenant put in no
          other. Every realm may use: {list.data?.openPools.join(", ") || "no pool"}.
        </p>
        <Cli cmd="realm list" />
      </Card>
      <Card title="Pools' realms">
        <Table head={["Pool", "Realms that may use it", ""]}>
          {pools.map((p) => {
            const rs = poolRealms(p.id);
            return (
              <tr key={p.id} data-testid={`tenancy-pool-${p.id}`}>
                <td className={td}>
                  {p.id} {p.name}
                </td>
                <td className={td}>{rs.length ? rs.join(", ") : "every realm"}</td>
                <td className={td}>
                  {can("store") && <Button onClick={() => open({ kind: "pool", pool: p.id, name: p.name, realms: rs })}>Change</Button>}
                </td>
              </tr>
            );
          })}
        </Table>
      </Card>
      <Card
        title="Hosts"
        actions={can("secure") && <Button onClick={() => open({ kind: "host", host: "", realms: [], isNew: true })}>Bind a host</Button>}
      >
        <Table head={["Host", "Realms", ""]} empty="No host is bound: every host attaches volumes of any realm.">
          {[...hosts.entries()].map(([h, rs]) => (
            <tr key={h} data-testid={`tenancy-host-${h}`}>
              <td className={td}>{h}</td>
              <td className={td}>{rs.join(", ")}</td>
              <td className={`${td} flex gap-2`}>
                {can("secure") && (
                  <>
                    <Button onClick={() => open({ kind: "host", host: h, realms: rs, isNew: false })}>Change</Button>
                    <Button onClick={() => open({ kind: "host", host: h, realms: [], isNew: false })}>Unbind</Button>
                  </>
                )}
              </td>
            </tr>
          ))}
        </Table>
        <p className="text-xs text-slate-500 mt-2">
          A bound host attaches only volumes of its realms (add default for volumes of no other); an S3 gateway is bound by
          its certificate&apos;s name. Hosts are named as they name themselves (hyperstor-vhost and hyperstor-ublk --host).
        </p>
      </Card>
      <Card
        title="Management clients"
        actions={can("secure") && <Button onClick={() => open({ kind: "client", client: "", realms: [], isNew: true })}>Bind a client</Button>}
      >
        <Table head={["Client", "Realms", "Join tokens", ""]} empty="No management client is bound: each acts in the whole cluster.">
          {clients.map((c) => (
            <tr key={c.client} data-testid={`tenancy-client-${c.client}`}>
              <td className={td}>{c.client}</td>
              <td className={td}>{c.realms.join(", ") || "unbound"}</td>
              <td className={td} data-testid={`tenancy-client-tokens-${c.client}`}>
                {c.tokenRoles.join(", ") || "–"}
              </td>
              <td className={`${td} flex gap-2`}>
                {can("secure") && (
                  <>
                    <Button
                      onClick={() => open({ kind: "clientTokens", client: c.client, allow: !c.tokenRoles.includes("host") })}
                    >
                      {c.tokenRoles.includes("host") ? "No host tokens" : "Allow host tokens"}
                    </Button>
                    <Button onClick={() => open({ kind: "client", client: c.client, realms: c.realms, isNew: false })}>Change</Button>
                    <Button onClick={() => open({ kind: "client", client: c.client, realms: [], isNew: false })}>Unbind</Button>
                  </>
                )}
              </td>
            </tr>
          ))}
        </Table>
        <p className="text-xs text-slate-500 mt-2">
          A management client (a mgmt identity such as glidex&apos;s datastore driver, by its certificate&apos;s name) bound to
          realms acts only in them. Bound, it makes join tokens only for VM hosts, and only once allowed here; unbound,
          it makes any token.
        </p>
      </Card>
      {dialog?.kind === "create" && (
        <ActionDialog
          title="Create a realm"
          submit="Create"
          cli={`realm create --name ${name || "N"}${description ? ` --description '${description}'` : ""}`}
          run={() => realms.createRealm({ name: name.trim(), description })}
          onClose={close}
        >
          <Field label="Name (1–63 of a–z, 0–9 and -)">
            <input className={input} value={name} onChange={(e) => setName(e.target.value)} autoFocus />
          </Field>
          <Field label="Description (optional)">
            <input className={input} value={description} onChange={(e) => setDescription(e.target.value)} />
          </Field>
        </ActionDialog>
      )}
      {dialog?.kind === "delete" && (
        <TypedDelete
          title={`Delete realm ${dialog.realm.name}`}
          name={dialog.realm.name}
          cli={`realm delete --name ${dialog.realm.name}`}
          run={() => realms.deleteRealm({ name: dialog.realm.name })}
          onClose={close}
        >
          <p className="text-sm">Refused while a tenant is in it, a pool&apos;s list names it or it has an S3 placement.</p>
        </TypedDelete>
      )}
      {dialog?.kind === "pool" && (
        <ActionDialog
          title={`Realms of pool ${dialog.pool} ${dialog.name}`}
          submit="Save"
          cli={`realm pool --pool ${dialog.pool} --realm ${realmArg(picked)}`}
          run={() => realms.setPoolRealms({ pool: dialog.pool, realms: picked })}
          onClose={close}
        >
          <RealmBoxes all={names} picked={picked} onChange={setPicked} none="every realm may use it" />
          <p className="text-xs text-slate-500">Refused while a placement of a realm taken off names the pool.</p>
        </ActionDialog>
      )}
      {dialog?.kind === "host" && (
        <ActionDialog
          title={dialog.isNew ? "Bind a host" : `Realms of host ${dialog.host}`}
          submit={!dialog.isNew && picked.length === 0 ? "Unbind" : "Save"}
          cli={`realm host --host ${(dialog.isNew ? name : dialog.host) || "H"} --realm ${realmArg(picked)}`}
          run={() => realms.setHostRealms({ host: dialog.isNew ? name.trim() : dialog.host, realms: picked })}
          onClose={close}
        >
          {dialog.isNew && (
            <Field label="Host (the name it gives, its hostname by default)">
              <input className={input} value={name} onChange={(e) => setName(e.target.value)} autoFocus />
            </Field>
          )}
          <RealmBoxes all={names} picked={picked} onChange={setPicked} none="unbound, any realm" />
          <p className="text-xs text-slate-500">This needs a login of the last 10 minutes.</p>
        </ActionDialog>
      )}
      {dialog?.kind === "client" && (
        <ActionDialog
          title={dialog.isNew ? "Bind a management client" : `Realms of client ${dialog.client}`}
          submit={!dialog.isNew && picked.length === 0 ? "Unbind" : "Save"}
          cli={`realm client --client ${(dialog.isNew ? name : dialog.client) || "C"} --realm ${realmArg(picked)}`}
          run={() => realms.setClientRealms({ client: dialog.isNew ? name.trim() : dialog.client, realms: picked })}
          onClose={close}
        >
          {dialog.isNew && (
            <Field label="Client (the name in its mgmt certificate)">
              <input className={input} value={name} onChange={(e) => setName(e.target.value)} autoFocus />
            </Field>
          )}
          <RealmBoxes all={names} picked={picked} onChange={setPicked} none="unbound, the whole cluster" />
          <p className="text-xs text-slate-500">This needs a login of the last 10 minutes.</p>
        </ActionDialog>
      )}
      {dialog?.kind === "clientTokens" && (
        <ActionDialog
          title={`Host tokens of client ${dialog.client}`}
          submit={dialog.allow ? "Allow" : "Take away"}
          cli={`realm client-tokens --client ${dialog.client} --role ${dialog.allow ? "host" : "none"}`}
          run={() => realms.setClientTokenRoles({ client: dialog.client, roles: dialog.allow ? ["host"] : [] })}
          onClose={close}
        >
          <p className="text-sm">
            {dialog.allow
              ? "While bound to realms, this client may then make join tokens for VM hosts, each giving a host a certificate under the name the client chose. It makes no other token."
              : "While bound to realms, this client then makes no join token. Tokens it made stay valid until used, revoked or expired."}
          </p>
          <p className="text-xs text-slate-500">This needs a login of the last 10 minutes.</p>
        </ActionDialog>
      )}
    </>
  );
}

// ------------------------------------------------- Tenants and projects

type Tone = "ok" | "warn" | "error" | "muted";

/** An operation's state as the Operations page shows it. */
const OP_STATE: Record<number, [string, Tone]> = {
  [OperationState.RUNNING]: ["running", "warn"],
  [OperationState.DONE]: ["done", "ok"],
  [OperationState.FAILED]: ["failed", "error"],
  [OperationState.CANCELLED]: ["cancelled", "muted"],
};

/** A move under way: the operation's progress until it ends. */
function OpProgress({ id, what, onEnd }: { id: string; what: string; onEnd: () => void }) {
  const [ended, setEnded] = useState(false);
  const op = usePoll(() => ops.getOperation({ id }), ended ? 0 : 1000);
  const o = op.data;
  const running = !o || o.state === OperationState.RUNNING;
  useEffect(() => {
    if (!running && !ended) {
      setEnded(true);
      onEnd();
    }
  }, [running, ended, onEnd]);
  const [text, tone]: [string, Tone] = o ? (OP_STATE[o.state] ?? ["unknown", "muted"]) : ["starting", "warn"];
  return (
    <div className="flex flex-wrap items-center gap-2 text-sm" data-testid={`tenancy-op-${id}`}>
      <span>
        {what}: operation <span className="font-mono text-xs">{id}</span>
      </span>
      <Pill tone={tone}>{text}</Pill>
      {o && o.total > 0 && (
        <span className="flex items-center gap-2">
          <span className="w-24 h-2 rounded-sm bg-slate-200 dark:bg-slate-700 overflow-hidden inline-block">
            <span className="block h-full bg-sky-500" style={{ width: `${(100 * o.done) / o.total}%` }} />
          </span>
          {o.done}/{o.total}
        </span>
      )}
      {o?.message && <span className="text-slate-600 dark:text-slate-400">{o.message}</span>}
      <Link className="text-sky-700 dark:text-sky-300 hover:underline" to="/operations">
        Operations
      </Link>
      <ErrorText error={op.error} />
    </div>
  );
}

/** The space rows of one tenant or project, summed over pools (one copy
 * each), with each pool's split in the tooltip. */
function SpaceCell({ rows, testId }: { rows: SpaceRow[]; testId: string }) {
  if (rows.length === 0) return <span className="text-slate-500">–</span>;
  const sum = (f: (r: SpaceRow) => bigint) => rows.reduce((a, r) => a + f(r), 0n);
  const title = rows
    .map(
      (r) =>
        `pool ${r.pool} (${r.copies} copies): head ${fmtBytes(r.headBytes)}, snapshots ${fmtBytes(r.snapshotBytes)}, trash ${fmtBytes(r.trashBytes)}`,
    )
    .join("\n");
  return (
    <span data-testid={testId} title={title}>
      {fmtBytes(sum((r) => r.usedBytes))}
      <span className="text-xs text-slate-500">
        {" "}
        (head {fmtBytes(sum((r) => r.headBytes))}, snapshots {fmtBytes(sum((r) => r.snapshotBytes))}, trash{" "}
        {fmtBytes(sum((r) => r.trashBytes))})
      </span>
    </span>
  );
}

/** Where a linked record lives: a badge naming its directory, linked to
 * its page there (`/tenants/<id>`, `/projects/<id>` at the issuer's
 * origin, D310). */
function DirectoryBadge({ issuer, path }: { issuer: string; path: string }) {
  const origin = directoryOrigin(issuer);
  const label = `from directory ${directoryName(issuer)}`;
  return (
    <span className="inline-flex items-center gap-1" title={`${issuer}: its name and membership are the directory's`}>
      <Pill tone="muted">{label}</Pill>
      {origin && (
        <a className="text-xs text-sky-700 dark:text-sky-300 hover:underline" href={`${origin}${path}`} target="_blank" rel="noreferrer">
          open in glidex
        </a>
      )}
    </span>
  );
}

type TenantDialog =
  | { kind: "create-tenant"; id?: number }
  | { kind: "rename-tenant"; t: Tenant }
  | { kind: "realm"; t: Tenant }
  | { kind: "delete-tenant"; t: Tenant }
  | { kind: "create-project"; t: Tenant }
  | { kind: "rename-project"; p: Project }
  | { kind: "move"; p: Project; from: Tenant }
  | { kind: "delete-project"; p: Project; t: Tenant };

function TenantsTab() {
  const can = useCan();
  const list = usePoll(async () => (await tenants.listTenants({})).tenants, 5000);
  const realmList = usePoll(() => realms.listRealms({}), 5000);
  // The sums per tenant and project, without a row per volume (I413);
  // space is a read role's, not a security administrator's.
  const reads = can("read");
  const space = usePoll(async () => (reads ? await cluster.getSpace({ pool: 0, skipVolumes: true }) : null), 10000);
  const [dialog, setDialog] = useState<TenantDialog | null>(null);
  const [name, setName] = useState("");
  const [target, setTarget] = useState("");
  const [moves, setMoves] = useState<{ id: string; what: string }[]>([]);
  const all = list.data ?? [];
  const realmNames = (realmList.data?.realms ?? []).map((r) => r.name);
  const reload = list.reload;
  const close = (done: boolean) => {
    setDialog(null);
    if (done) list.reload();
  };
  const open = (d: TenantDialog) => {
    setName(d.kind === "rename-tenant" ? d.t.name : d.kind === "rename-project" ? d.p.name : "");
    setTarget(d.kind === "realm" ? d.t.realm || "default" : "");
    setDialog(d);
  };
  const tenantSpace = (id: number) => (space.data?.tenants ?? []).filter((r) => r.tenant === id);
  const projectSpace = (id: number) => (space.data?.projects ?? []).filter((r) => r.project === id);
  const linked = (t: Tenant) => t.directory !== "";
  const realmOfTenant = (t: Tenant) => t.realm || "default";
  // Realms in ListRealms' order (default first), then any a tenant names
  // that the list doesn't (it changed between the two reads).
  const order = [...new Set([...realmNames, ...all.map(realmOfTenant)])];
  const known = new Set(all.map((t) => t.id));
  const rows: ReactNode[] = [];
  for (const realm of order) {
    const ts = all.filter((t) => realmOfTenant(t) === realm);
    // Tenants used before records (on volumes, buckets or keys): their
    // numbers, to adopt (CreateTenant with an id).
    const bare = (realmList.data?.realms.find((r) => r.name === realm)?.tenants ?? []).filter((id) => !known.has(id));
    rows.push(
      <tr key={`realm-${realm}`} data-testid={`tree-realm-${realm}`} className="bg-slate-50 dark:bg-slate-800/50">
        <td className={`${td} font-semibold`} colSpan={4}>
          realm {realm}
        </td>
      </tr>,
    );
    if (ts.length === 0 && bare.length === 0)
      rows.push(
        <tr key={`realm-${realm}-empty`}>
          <td className={`${td} pl-6 text-slate-500`} colSpan={4}>
            No tenant.
          </td>
        </tr>,
      );
    for (const id of bare)
      rows.push(
        <tr key={`bare-${id}`} data-testid={`tree-bare-${id}`}>
          <td className={`${td} pl-6`}>
            tenant {id} <span className="text-xs text-slate-500">(used before records; no name)</span>
          </td>
          <td className={td} />
          <td className={td} />
          <td className={td}>{can("store") && <Button onClick={() => open({ kind: "create-tenant", id })}>Name it</Button>}</td>
        </tr>,
      );
    for (const t of ts) {
      rows.push(
        <tr key={`t-${t.id}`} data-testid={`tree-tenant-${t.name}`}>
          <td className={`${td} pl-6`}>
            <span className="font-medium">{t.name}</span> <span className="text-xs text-slate-500">tenant {t.id}</span>{" "}
            {linked(t) && <DirectoryBadge issuer={t.directory} path={`/tenants/${t.glidexId}`} />}
          </td>
          <td className={td}>{t.projects.length}</td>
          <td className={td}>
            <SpaceCell rows={tenantSpace(t.id)} testId={`space-tenant-${t.name}`} />
          </td>
          <td className={`${td} flex flex-wrap gap-2`}>
            {can("store") && <Button onClick={() => open({ kind: "create-project", t })}>New project</Button>}
            {!linked(t) && can("store") && <Button onClick={() => open({ kind: "rename-tenant", t })}>Rename</Button>}
            {!linked(t) && can("secure") && <Button onClick={() => open({ kind: "realm", t })}>Realm</Button>}
            {!linked(t) && can("store") && (
              <Button kind="danger" onClick={() => open({ kind: "delete-tenant", t })}>
                Delete
              </Button>
            )}
          </td>
        </tr>,
      );
      for (const p of t.projects) {
        // A project the directory links is its; a local one in a linked
        // tenant (made here, or unlinked by the feed, D316) is ours.
        const mine = p.glidexId === "";
        rows.push(
          <tr key={`p-${p.id}`} data-testid={`tree-project-${t.name}-${p.name}`}>
            <td className={`${td} pl-12`}>
              {p.name} <span className="text-xs text-slate-500">project {p.id}</span>
              {p.isDefault && (
                <>
                  {" "}
                  <Pill tone="muted">default</Pill>
                </>
              )}
              {p.state !== "active" && (
                <>
                  {" "}
                  <Pill tone="warn">{p.state}</Pill>
                </>
              )}{" "}
              {!mine && linked(t) && <DirectoryBadge issuer={t.directory} path={`/projects/${p.glidexId}`} />}
            </td>
            <td className={td} />
            <td className={td}>
              <SpaceCell rows={projectSpace(p.id)} testId={`space-project-${t.name}-${p.name}`} />
            </td>
            <td className={`${td} flex flex-wrap gap-2`}>
              {mine && !p.isDefault && can("store") && (
                <>
                  <Button onClick={() => open({ kind: "rename-project", p })}>Rename</Button>
                  <Button onClick={() => open({ kind: "move", p, from: t })}>Move</Button>
                  <Button kind="danger" onClick={() => open({ kind: "delete-project", p, t })}>
                    Delete
                  </Button>
                </>
              )}
            </td>
          </tr>,
        );
      }
    }
  }
  return (
    <>
      <Card title="Tenants and projects" actions={can("store") && <Button onClick={() => open({ kind: "create-tenant" })}>Create tenant</Button>}>
        <ErrorText error={list.error ?? realmList.error} />
        {moves.map((m) => (
          <OpProgress key={m.id} id={m.id} what={m.what} onEnd={reload} />
        ))}
        <Table head={["Realm, tenant, project", "Projects", "Space used (one copy)", ""]} empty="No tenants.">
          {rows}
        </Table>
        {space.error ? (
          <p className="text-xs text-slate-500 mt-2">Space isn&apos;t known now: {(space.error as Error).message}</p>
        ) : (
          space.data &&
          !space.data.complete && (
            <p className="text-xs text-slate-500 mt-2">Space is still being counted (not every PG has reported yet).</p>
          )
        )}
        <p className="text-xs text-slate-500 mt-2">
          A tenant is an organization&apos;s account; its projects own volumes and buckets. A tenant or project linked to a
          glidex directory takes its name and membership from the directory&apos;s feed, so those are changed there.
        </p>
        <Cli cmd="tenant list" />
      </Card>
      {dialog?.kind === "create-tenant" && (
        <ActionDialog
          title={dialog.id ? `Name tenant ${dialog.id}` : "Create a tenant"}
          submit="Create"
          cli={`tenant create --name ${name || "N"}${dialog.id ? ` --tenant ${dialog.id}` : ""}`}
          run={() => tenants.createTenant({ name: name.trim(), id: dialog.id ?? 0 })}
          onClose={close}
        >
          <Field label="Name (1–63 of a–z, 0–9 and -; not default)">
            <input className={input} value={name} onChange={(e) => setName(e.target.value)} autoFocus />
          </Field>
          <p className="text-xs text-slate-500">It gets a project &ldquo;default&rdquo;, and is in realm default.</p>
        </ActionDialog>
      )}
      {dialog?.kind === "rename-tenant" && (
        <ActionDialog
          title={`Rename tenant ${dialog.t.name}`}
          submit="Rename"
          cli={`tenant rename --tenant ${dialog.t.id} --name ${name || "N"}`}
          run={() => tenants.renameTenant({ id: dialog.t.id, name: name.trim() })}
          onClose={close}
        >
          <Field label="New name">
            <input className={input} value={name} onChange={(e) => setName(e.target.value)} autoFocus />
          </Field>
        </ActionDialog>
      )}
      {dialog?.kind === "realm" && (
        <ActionDialog
          title={`Tenant ${dialog.t.name}'s realm`}
          submit="Move"
          cli={`realm tenant --tenant ${dialog.t.id} --realm ${target}`}
          run={() => realms.setTenantRealm({ tenant: dialog.t.id, realm: target })}
          onClose={close}
        >
          <Field label="Realm">
            <select className={input} value={target} onChange={(e) => setTarget(e.target.value)}>
              {realmNames.map((r) => (
                <option key={r} value={r}>
                  {r}
                </option>
              ))}
            </select>
          </Field>
          <p className="text-xs text-slate-500">
            Its keys then work only at that realm&apos;s gateways. Refused while any of its volumes or buckets is in a pool the
            realm may not use. This needs a login of the last 10 minutes.
          </p>
        </ActionDialog>
      )}
      {dialog?.kind === "delete-tenant" && (
        <TypedDelete
          title={`Delete tenant ${dialog.t.name}`}
          name={dialog.t.name}
          cli={`tenant delete --tenant ${dialog.t.id}`}
          run={() => tenants.deleteTenant({ id: dialog.t.id })}
          onClose={close}
        >
          <p className="text-sm">
            Refused while it has a project besides default, a volume (in the trash too), a bucket or an S3 key, or is in a realm
            other than default.
          </p>
        </TypedDelete>
      )}
      {dialog?.kind === "create-project" && (
        <ActionDialog
          title={`New project of tenant ${dialog.t.name}`}
          submit="Create"
          cli={`project create --tenant ${dialog.t.id} --name ${name || "N"}`}
          run={() => tenants.createProject({ tenant: dialog.t.id, name: name.trim() })}
          onClose={close}
        >
          <Field label="Name (1–63 of a–z, 0–9 and -; unique in the tenant)">
            <input className={input} value={name} onChange={(e) => setName(e.target.value)} autoFocus />
          </Field>
          {linked(dialog.t) && (
            <p className="text-xs text-slate-500">A local project: the directory&apos;s feed neither lists nor changes it.</p>
          )}
        </ActionDialog>
      )}
      {dialog?.kind === "rename-project" && (
        <ActionDialog
          title={`Rename project ${dialog.p.name}`}
          submit="Rename"
          cli={`project rename --project ${dialog.p.id} --name ${name || "N"}`}
          run={() => tenants.renameProject({ id: dialog.p.id, name: name.trim() })}
          onClose={close}
        >
          <Field label="New name">
            <input className={input} value={name} onChange={(e) => setName(e.target.value)} autoFocus />
          </Field>
        </ActionDialog>
      )}
      {dialog?.kind === "move" && (
        <ActionDialog
          title={`Move project ${dialog.from.name}/${dialog.p.name}`}
          submit="Move"
          cli={`project move --project ${dialog.p.id} --to ${target || "N"}`}
          run={async () => {
            if (!target) throw new Error("Pick the tenant it goes to");
            const to = all.find((t) => t.id === Number(target));
            const r = await tenants.moveProject({ project: dialog.p.id, to: Number(target) });
            if (r.operation?.id)
              setMoves((m) => [...m, { id: r.operation!.id, what: `Moving ${dialog.from.name}/${dialog.p.name} to ${to?.name ?? target}` }]);
          }}
          onClose={close}
        >
          <Field label="To tenant">
            <select className={input} value={target} onChange={(e) => setTarget(e.target.value)}>
              <option value="">pick one</option>
              {all
                .filter((t) => t.id !== dialog.from.id)
                .map((t) => (
                  <option key={t.id} value={t.id}>
                    {t.name} (realm {realmOfTenant(t)})
                  </option>
                ))}
            </select>
          </Field>
          <p className="text-xs text-slate-500">
            It runs as an operation: its buckets take the new tenant&apos;s name, keeping their objects, then the project and its
            volumes are the new tenant&apos;s. Refused first unless the new tenant&apos;s realm may use its pools and has none of its
            bucket names; across realms, its volumes must be detached while hosts are bound. It can&apos;t be cancelled. This needs
            a login of the last 10 minutes.
          </p>
        </ActionDialog>
      )}
      {dialog?.kind === "delete-project" && (
        <TypedDelete
          title={`Delete project ${dialog.t.name}/${dialog.p.name}`}
          name={dialog.p.name}
          cli={`project delete --project ${dialog.p.id}`}
          run={() => tenants.deleteProject({ id: dialog.p.id })}
          onClose={close}
        >
          <p className="text-sm">Refused while a volume (in the trash too) or a bucket is in it.</p>
        </TypedDelete>
      )}
    </>
  );
}

// ------------------------------------------------------------ Directories

/** A cap on a tenant's bytes in a pool (D347): kept, not yet enforced. */
type Cap = { pool: string; bytes: bigint };

/** Caps as hyperstorctl's `--cap` takes them, or `none`. */
const capsArg = (cs: Cap[]) => (cs.length ? cs.map((c) => `${c.pool}=${c.bytes}`).join(",") : "none");

/** Caps for people to read. */
const capsText = (cs: Cap[]) => (cs.length ? cs.map((c) => `${c.pool} ${fmtBytes(c.bytes)}`).join(", ") : "none");

/** Caps typed as `POOL=SIZE, …` (sizes as `1G`, `512M`); empty or `none`:
 * none. Null when one doesn't read, or a size is 0. */
function parseCaps(s: string): Cap[] | null {
  const t = s.trim();
  if (t === "" || t === "none") return [];
  const out: Cap[] = [];
  for (const part of t.split(",")) {
    const [pool, size, ...rest] = part.split("=");
    const bytes = size === undefined ? null : parseSize(size);
    if (rest.length || !pool.trim() || bytes === null || bytes === 0n) return null;
    out.push({ pool: pool.trim(), bytes });
  }
  return out;
}

type DirDialog =
  | { kind: "staleness"; d: TrustedDirectory }
  | { kind: "admission"; d: TrustedDirectory }
  | { kind: "decide"; d: TrustedDirectory; slug: string; approve: boolean }
  | { kind: "endWhole"; d: TrustedDirectory }
  | { kind: "end"; d: TrustedDirectory; slug: string; name: string }
  | { kind: "operator"; tenant: number; name: string }
  | { kind: "offer" }
  | { kind: "revoke"; o: TrustOffer };

/** The default staleness limit, in minutes (D309, D324). */
const DEFAULT_STALENESS = 60;

function DirectoriesTab() {
  const can = useCan();
  const dirs = usePoll(() => trust.listTrustedDirectories({}), 5000);
  const offers = usePoll(async () => (await trust.listOffers({})).offers, 5000);
  const [dialog, setDialog] = useState<DirDialog | null>(null);
  const [minutes, setMinutes] = useState("");
  // The offer's fields. Its token is kept only in `made`, while the
  // dialog showing it is open: never in storage, a URL or a log.
  // `whole`: an offer to the directory as a whole (D345), with its
  // admission setting and default caps (D346, D347).
  const blank = { tenant: "", directory: "", name: "", realm: "", url: "", pin: "", days: "7", ui: "", whole: false, admit: "open", caps: "" };
  const [f, setF] = useState(blank);
  // A whole directory's setting and default caps, as changed (D347).
  const [adm, setAdm] = useState({ admit: "open", caps: "" });
  const [made, setMade] = useState<{ token: string; offer?: TrustOffer } | null>(null);
  // The pin mgmt gives the offer's address (D343), unless another is
  // pasted ("Use another").
  const [own, setOwn] = useState<{ pin?: string; error?: unknown }>({});
  const [other, setOther] = useState(false);
  const offering = dialog?.kind === "offer";
  useEffect(() => {
    if (!offering || other) return;
    let live = true;
    offerPin(f.url.trim())
      .then((pin) => live && setOwn({ pin }))
      .catch((error) => live && setOwn({ error }));
    return () => {
      live = false;
    };
  }, [offering, other, f.url]);
  const operator = dirs.data?.operator ?? 0;
  const operatorName = dirs.data?.directories.flatMap((d) => d.tenants).find((t) => t.tenant === operator && operator !== 0)?.name;
  const now = Date.now() / 1000;
  const close = (done: boolean) => {
    setDialog(null);
    if (done) {
      dirs.reload();
      offers.reload();
    }
  };
  const offerCaps = f.whole ? parseCaps(f.caps) : [];
  const admCaps = parseCaps(adm.caps);
  const offerCli = () =>
    [
      f.whole
        ? `admission offer --directory ${f.directory || "ISSUER"} --url ${f.url || "URL"}`
        : `admission offer --tenant ${f.tenant || "SLUG"} --directory ${f.directory || "ISSUER"} --url ${f.url || "URL"}`,
      other && f.pin && `--pin ${f.pin}`,
      !f.whole && f.name && `--name ${f.name}`,
      f.whole && f.admit !== "open" && `--admit ${f.admit}`,
      f.whole && offerCaps && offerCaps.length > 0 && `--cap ${capsArg(offerCaps)}`,
      f.realm && `--realm ${f.realm}`,
      f.days && f.days !== "7" && `--days ${f.days}`,
      f.ui && `--ui ${f.ui}`,
    ]
      .filter(Boolean)
      .join(" ");
  const set = (k: keyof typeof f) => (e: { target: { value: string } }) => setF((x) => ({ ...x, [k]: e.target.value }));
  return (
    <>
      <Card title="Trusted directories">
        <ErrorText error={dirs.error} />
        {(dirs.data?.directories ?? []).length === 0 && !dirs.error && (
          <p className="text-sm text-slate-500">
            No directory is trusted: a directory&apos;s tenant, or for the whole directory its owner, redeems an offer (below) first.
          </p>
        )}
        <div className="grid gap-4">
          {(dirs.data?.directories ?? []).map((d) => {
            const limit = d.maxStalenessMinutes || DEFAULT_STALENESS;
            const age = d.checkedUnix ? now - Number(d.checkedUnix) : null;
            const stale = age == null || age > limit * 60;
            return (
              <div key={d.id} className="border border-slate-200 dark:border-slate-700 rounded-lg p-3 grid gap-2" data-testid={`directory-${directoryName(d.issuer)}`}>
                <div className="flex flex-wrap items-center gap-2 text-sm">
                  <span className="font-medium">{d.issuer}</span>
                  <span className="text-xs text-slate-500 font-mono">{d.id}</span>
                  <Pill tone="muted">realm {d.realm}</Pill>
                  <Pill tone={stale ? "error" : "ok"}>{stale ? "feed stale: its people are refused" : "feed fresh"}</Pill>
                </div>
                <div className="text-sm grid gap-1">
                  <span>
                    Feed last answered {fmtTime(d.checkedUnix)}, last changed {fmtTime(d.appliedUnix)}
                    {d.revision && <span className="text-xs text-slate-500"> (revision {d.revision})</span>}
                  </span>
                  <span data-testid={`staleness-${directoryName(d.issuer)}`}>
                    Its people are refused {limit} minutes after its feed last answered
                    {d.maxStalenessMinutes ? "" : " (the cluster's default)"}.{" "}
                    {can("secure") && (
                      <Button
                        onClick={() => {
                          setMinutes(String(d.maxStalenessMinutes));
                          setDialog({ kind: "staleness", d });
                        }}
                      >
                        Change
                      </Button>
                    )}
                  </span>
                  {d.uiCallback && <span className="text-xs text-slate-500">Logins come back to {d.uiCallback}</span>}
                  {d.blocked.length > 0 && (
                    <span className="text-amber-700 dark:text-amber-400">Not applied yet (retried at every pull): {d.blocked.join("; ")}</span>
                  )}
                </div>
                {d.whole && (
                  <div className="text-sm grid gap-1" data-testid={`whole-${directoryName(d.issuer)}`}>
                    <span className="flex flex-wrap items-center gap-2">
                      <Pill tone="ok">trusted as a whole</Pill>
                      <span>
                        Its tenants opt in at the directory:{" "}
                        {d.admit === "approval" ? "each waits for a security administrator's approval" : "each is admitted at once"}. Each admitted
                        tenant starts with caps {capsText(d.caps)}.
                      </span>
                    </span>
                    {can("secure") && (
                      <span className="flex gap-2">
                        <Button
                          onClick={() => {
                            setAdm({ admit: d.admit || "open", caps: d.caps.length ? capsArg(d.caps) : "" });
                            setDialog({ kind: "admission", d });
                          }}
                        >
                          Change admission
                        </Button>
                        <Button kind="danger" onClick={() => setDialog({ kind: "endWhole", d })}>
                          End trust in the directory
                        </Button>
                      </span>
                    )}
                    {d.pending.length + d.refused.length > 0 && (
                      <Table head={["Opted in at the directory", "Decision", ""]}>
                        {[...d.pending, ...d.refused].map((t) => (
                          <tr key={t.id} data-testid={`admission-${t.slug}`}>
                            <td className={td}>
                              {t.slug} <span className="text-xs text-slate-500 font-mono">{t.id}</span>
                            </td>
                            <td className={td}>
                              <Pill tone={t.admission === "pending" ? "warn" : "muted"}>{t.admission}</Pill>
                            </td>
                            <td className={`${td} flex gap-2`}>
                              {can("secure") && t.admission === "pending" && (
                                <>
                                  <Button onClick={() => setDialog({ kind: "decide", d, slug: t.slug, approve: true })}>Approve</Button>
                                  <Button kind="danger" onClick={() => setDialog({ kind: "decide", d, slug: t.slug, approve: false })}>
                                    Refuse
                                  </Button>
                                </>
                              )}
                            </td>
                          </tr>
                        ))}
                      </Table>
                    )}
                  </div>
                )}
                <Table head={["Tenant at the directory", "Tenant here", "Operator", ""]}>
                  {d.tenants.map((t) => (
                    <tr key={t.id} data-testid={`trusted-${t.slug}`}>
                      <td className={td}>
                        {t.slug} <span className="text-xs text-slate-500 font-mono">{t.id}</span>
                      </td>
                      <td className={td}>
                        {t.tenant ? `${t.name} (tenant ${t.tenant})` : "–"}
                        {t.caps.length > 0 && <span className="text-xs text-slate-500"> caps {capsText(t.caps)}</span>}
                      </td>
                      <td className={td}>{t.operator ? <Pill tone="ok">operator</Pill> : "–"}</td>
                      <td className={`${td} flex gap-2`}>
                        {can("secure") && t.tenant !== 0 && !t.operator && (
                          <Button onClick={() => setDialog({ kind: "operator", tenant: t.tenant, name: t.name })}>Make operator</Button>
                        )}
                        {can("secure") && (
                          <Button kind="danger" onClick={() => setDialog({ kind: "end", d, slug: t.slug, name: t.name })}>
                            End trust
                          </Button>
                        )}
                      </td>
                    </tr>
                  ))}
                </Table>
              </div>
            );
          })}
        </div>
        <p className="text-sm mt-3" data-testid="operator-tenant">
          Operator tenant: {operator ? `${operatorName ?? "tenant"} (tenant ${operator})` : "none"}{" "}
          {can("secure") && operator !== 0 && <Button onClick={() => setDialog({ kind: "operator", tenant: 0, name: "" })}>Clear</Button>}
        </p>
        <Cli cmd="admission directories" />
      </Card>
      <Card
        title="Offers"
        actions={
          can("secure") && (
            <Button
              onClick={() => {
                setF({ ...blank, url: window.location.origin });
                setOther(false);
                setOwn({});
                setDialog({ kind: "offer" });
              }}
            >
              Make an offer
            </Button>
          )
        }
      >
        <ErrorText error={offers.error} />
        <Table head={["Id", "State", "For", "Name here", "Realm", "Directory", "Expires", "By", ""]} empty="No offers.">
          {(offers.data ?? []).map((o) => (
            <tr key={o.id} data-testid={`offer-${o.scope === "directory" ? `directory-${directoryName(o.directory)}` : o.tenant}`}>
              <td className={`${td} font-mono text-xs`}>{o.id}</td>
              <td className={td}>
                <Pill tone={o.state === "open" ? "warn" : o.state === "redeemed" ? "ok" : "muted"}>{o.state}</Pill>
              </td>
              <td className={td}>{o.scope === "directory" ? `the whole directory (${o.admit || "open"})` : o.tenant}</td>
              <td className={td}>{o.name}</td>
              <td className={td}>{o.realm || "–"}</td>
              <td className={`${td} text-xs`}>{o.directory}</td>
              <td className={td}>{fmtTime(o.expiresUnix)}</td>
              <td className={`${td} text-xs`}>{o.createdBy}</td>
              <td className={td}>
                {can("secure") && o.state === "open" && (
                  <Button kind="danger" onClick={() => setDialog({ kind: "revoke", o })}>
                    Revoke
                  </Button>
                )}
              </td>
            </tr>
          ))}
        </Table>
        <Cli cmd="admission list" />
      </Card>
      {dialog?.kind === "staleness" && (
        <ActionDialog
          title={`How long ${directoryName(dialog.d.issuer)} may go unheard`}
          submit="Save"
          cli={`admission staleness --directory ${dialog.d.issuer} --minutes ${minutes || "0"}`}
          run={() => trust.setDirectoryStaleness({ directory: dialog.d.id, minutes: Number(minutes || "0") })}
          onClose={close}
        >
          <Field label="Minutes (1 to 10080; 0: the cluster's default, an hour)">
            <input className={input} value={minutes} onChange={(e) => setMinutes(e.target.value.replace(/\D/g, ""))} autoFocus />
          </Field>
          <p className="text-xs text-slate-500">
            Past it since the feed last answered, the directory&apos;s sessions and tokens are refused, as a revocation may be
            missing; local logins work. This needs a login of the last 10 minutes.
          </p>
        </ActionDialog>
      )}
      {dialog?.kind === "end" && (
        <ActionDialog
          title={`End the trust in ${dialog.slug}`}
          submit="End trust"
          kind="danger"
          cli={`admission end --directory ${dialog.d.issuer} --tenant ${dialog.slug}`}
          run={() => trust.endTrust({ directory: dialog.d.id, tenant: dialog.slug })}
          onClose={close}
        >
          <p className="text-sm">
            {dialog.name ? `Tenant ${dialog.name} and its projects stay here, unlinked.` : "Its records here stay, unlinked."} mgmt
            tells the directory.{" "}
            {dialog.d.whole
              ? "The directory stays trusted as a whole, and the tenant may opt in again."
              : "With its last tenant, the directory is no longer trusted and its people can't sign in."}{" "}
            This needs a login of the last 10 minutes.
          </p>
        </ActionDialog>
      )}
      {dialog?.kind === "admission" && (
        <ActionDialog
          title={`How ${directoryName(dialog.d.issuer)}'s tenants are admitted`}
          submit="Save"
          disabled={admCaps === null}
          cli={`admission directory --directory ${dialog.d.issuer} --admit ${adm.admit} --cap ${admCaps ? capsArg(admCaps) : "…"}`}
          run={() => trust.setDirectoryAdmission({ directory: dialog.d.id, admit: adm.admit, setCaps: true, caps: admCaps ?? [] })}
          onClose={close}
        >
          <Field label="Opt-ins">
            <select className={input} value={adm.admit} onChange={(e) => setAdm((x) => ({ ...x, admit: e.target.value }))}>
              <option value="open">open: admitted at once</option>
              <option value="approval">approval: each waits for a security administrator</option>
            </select>
          </Field>
          <Field label="Default caps (POOL=SIZE, comma-separated; empty: none)">
            <input className={input} value={adm.caps} onChange={(e) => setAdm((x) => ({ ...x, caps: e.target.value }))} />
          </Field>
          {admCaps === null && <p className="text-xs text-red-700 dark:text-red-400">Caps read as POOL=SIZE, such as fast=100G.</p>}
          <p className="text-xs text-slate-500">
            The caps apply to tenants admitted from now on; those admitted keep theirs. Caps are kept, not yet enforced. This needs a
            login of the last 10 minutes.
          </p>
        </ActionDialog>
      )}
      {dialog?.kind === "decide" && (
        <ActionDialog
          title={`${dialog.approve ? "Approve" : "Refuse"} ${dialog.slug}'s opt-in`}
          submit={dialog.approve ? "Approve" : "Refuse"}
          kind={dialog.approve ? "primary" : "danger"}
          cli={`admission ${dialog.approve ? "approve" : "reject"} --directory ${dialog.d.issuer} --tenant ${dialog.slug}`}
          run={() => trust.decideAdmission({ directory: dialog.d.id, tenant: dialog.slug, approve: dialog.approve })}
          onClose={close}
        >
          <p className="text-sm">
            {dialog.approve
              ? `Its tenant is made here in realm ${dialog.d.realm} with a copy of the directory's default caps (${capsText(dialog.d.caps)}), and the feed brings its projects and members.`
              : "It isn't admitted, and the directory learns so from the next report. Its administrator may opt in again."}{" "}
            This needs a login of the last 10 minutes.
          </p>
        </ActionDialog>
      )}
      {dialog?.kind === "endWhole" && (
        <ActionDialog
          title={`End the trust in ${directoryName(dialog.d.issuer)}`}
          submit="End trust"
          kind="danger"
          cli={`admission end --directory ${dialog.d.issuer}`}
          run={() => trust.endTrust({ directory: dialog.d.id, tenant: "" })}
          onClose={close}
        >
          <p className="text-sm">
            Every admission of the directory ends: its tenants and their projects stay here, unlinked, and its people can&apos;t sign
            in. mgmt tells the directory. This needs a login of the last 10 minutes.
          </p>
        </ActionDialog>
      )}
      {dialog?.kind === "operator" && (
        <ActionDialog
          title={dialog.tenant ? `Make ${dialog.name} the operator tenant` : "Clear the operator tenant"}
          submit={dialog.tenant ? "Make operator" : "Clear"}
          cli={`cluster set-operator --tenant ${dialog.tenant}`}
          run={() => trust.setOperator({ tenant: dialog.tenant })}
          onClose={close}
        >
          <p className="text-sm">
            The operator tenant&apos;s directory administers this cluster (glidex&apos;s cluster set-operator). This needs a login
            of the last 10 minutes.
          </p>
        </ActionDialog>
      )}
      {dialog?.kind === "offer" && (
        <ActionDialog
          title={f.whole ? "Make an offer to a whole directory" : "Make an offer to a directory's tenant"}
          submit="Make"
          disabled={offerCaps === null}
          cli={offerCli()}
          run={async () => {
            const r = await trust.createOffer({
              tenant: f.whole ? "" : f.tenant.trim(),
              directory: f.directory.trim(),
              name: f.whole ? "" : f.name.trim(),
              admit: f.whole ? f.admit : "",
              caps: f.whole ? (offerCaps ?? []) : [],
              realm: f.realm.trim(),
              url: f.url.trim(),
              // Empty: mgmt takes its own certificate's (D343).
              pin: other ? f.pin.trim() : "",
              days: Number(f.days || "0"),
              ui: f.ui.trim(),
            });
            setMade({ token: r.token, offer: r.offer });
          }}
          onClose={close}
        >
          <fieldset className="flex gap-4 text-sm">
            <legend className="sr-only">Offer to</legend>
            <label className="flex items-center gap-1">
              <input type="radio" name="offer-scope" checked={!f.whole} onChange={() => setF((x) => ({ ...x, whole: false }))} />
              One tenant
            </label>
            <label className="flex items-center gap-1">
              <input type="radio" name="offer-scope" checked={f.whole} onChange={() => setF((x) => ({ ...x, whole: true }))} />
              The whole directory
            </label>
          </fieldset>
          {!f.whole && (
            <Field label="glidex tenant (its slug)">
              <input className={input} value={f.tenant} onChange={set("tenant")} autoFocus />
            </Field>
          )}
          <Field label="Directory (its issuer, https://host/d/NAME)">
            <input className={input} value={f.directory} onChange={set("directory")} />
          </Field>
          {f.whole ? (
            <>
              <Field label="Opt-ins">
                <select className={input} value={f.admit} onChange={set("admit")}>
                  <option value="open">open: admitted at once</option>
                  <option value="approval">approval: each waits for a security administrator</option>
                </select>
              </Field>
              <Field label="Default caps (POOL=SIZE, comma-separated; empty: none)">
                <input className={input} value={f.caps} onChange={set("caps")} />
              </Field>
              {offerCaps === null && <p className="text-xs text-red-700 dark:text-red-400">Caps read as POOL=SIZE, such as fast=100G.</p>}
              <p className="text-xs text-slate-500" data-testid="whole-offer-note">
                The directory&apos;s owner redeems it. Each of its tenants then opts in by itself, with no offer, and is listed
                here.
                {f.admit === "open" &&
                  " With open, any tenant of the directory is admitted when it asks: if the directory lets anyone make a tenant, choose approval."}
              </p>
            </>
          ) : (
            <Field label="Name here (default: the slug)">
              <input className={input} value={f.name} onChange={set("name")} />
            </Field>
          )}
          <Field label="Realm (a directory trusted for the first time; default: named after it)">
            <input className={input} value={f.realm} onChange={set("realm")} />
          </Field>
          <Field label="mgmt's address as the directory reaches it (https://host[:port])">
            <input className={input} value={f.url} onChange={set("url")} />
          </Field>
          {other ? (
            <>
              <Field label="SHA-256 of the certificate served there (64 hex digits)">
                <input className={`${input} font-mono`} value={f.pin} onChange={set("pin")} spellCheck={false} autoFocus />
              </Field>
              <p className="text-xs text-slate-500">
                For an address where mgmt doesn&apos;t serve its own certificate: hyperstorctl admission offer reads it from
                the address, or <code>openssl s_client -connect HOST:PORT &lt;/dev/null | openssl x509 -noout -fingerprint -sha256</code>.{" "}
                <button type="button" className="text-sky-700 dark:text-sky-300 hover:underline" onClick={() => setOther(false)}>
                  Use mgmt&apos;s
                </button>
              </p>
            </>
          ) : (
            <div className="grid gap-1 text-xs text-slate-500">
              <span>SHA-256 of the certificate mgmt serves there, which the directory pins (from mgmt, not this browser)</span>
              <div className="flex gap-2 items-center">
                <input
                  readOnly
                  aria-label="Pin"
                  className={`${input} font-mono text-xs flex-1`}
                  value={own.pin ?? ""}
                  placeholder={own.error ? "none: paste one" : "…"}
                />
                <Button
                  onClick={() => {
                    setF((x) => ({ ...x, pin: "" }));
                    setOther(true);
                  }}
                >
                  Use another
                </Button>
              </div>
              <ErrorText error={own.error} />
            </div>
          )}
          <div className="grid grid-cols-2 gap-2">
            <Field label="Lasts (days, 1 to 30)">
              <input className={input} value={f.days} onChange={(e) => setF((x) => ({ ...x, days: e.target.value.replace(/\D/g, "") }))} />
            </Field>
            <Field label="Where browsers reach mgmt (default: the address above)">
              <input className={input} value={f.ui} onChange={set("ui")} />
            </Field>
          </div>
          <p className="text-xs text-slate-500">Its token is shown once. This needs a login of the last 10 minutes.</p>
        </ActionDialog>
      )}
      {made && (
        <Modal title="The offer's token" onClose={() => setMade(null)}>
          <TokenReveal
            secret={made.token}
            label="offer token"
            warning={
              made.offer?.scope === "directory"
                ? "It is shown this once, and trusts the whole directory: pass it only to the directory's owner."
                : "It is shown this once, and admits one glidex tenant: pass it only to that tenant's administrator."
            }
            copiedHint="Copied. They redeem it at their directory."
          >
            <div data-testid="offer-made" className="text-xs text-slate-500">
              Offer {made.offer?.id} for {made.offer?.scope === "directory" ? "the whole directory" : made.offer?.tenant}, until{" "}
              {fmtTime(made.offer?.expiresUnix ?? 0n)}, pinning SHA-256 <span className="font-mono">{made.offer?.pin}</span>. At the
              directory:
              <code className="block mt-1 p-2 rounded-sm bg-slate-100 dark:bg-slate-800">
                {made.offer?.scope === "directory" ? "gxctl directory trust-cluster --offer TOKEN" : "gxctl tenant opt-in --offer TOKEN"}
              </code>
            </div>
          </TokenReveal>
        </Modal>
      )}
      {dialog?.kind === "revoke" && (
        <ActionDialog
          title={`Revoke the offer to ${dialog.o.scope === "directory" ? directoryName(dialog.o.directory) : dialog.o.tenant}`}
          submit="Revoke"
          kind="danger"
          cli={`admission revoke --id ${dialog.o.id}`}
          run={() => trust.revokeOffer({ id: dialog.o.id })}
          onClose={close}
        >
          <p className="text-sm">Its token can no longer be redeemed.</p>
        </ActionDialog>
      )}
    </>
  );
}

// ------------------------------------------------------------------ Page

export default function Tenancy() {
  const can = useCan();
  // Security administrators read realms and tenants too (I413).
  const tabs = [
    ...(can("read") || can("secure") ? (["Realms", "Tenants & projects"] as const) : []),
    ...(can("secure") ? (["Directories"] as const) : []),
  ];
  const [tab, setTab] = useState<string>(tabs[0] ?? "");
  if (tabs.length === 0)
    return (
      <Card title="Tenancy">
        <p className="text-sm text-slate-600">
          Realms, tenants and glidex directories are read with a role: viewer, operator, storage-admin or
          security-admin.
        </p>
      </Card>
    );
  return (
    <>
      <div className="flex gap-1" role="tablist">
        {tabs.map((t) => (
          <button
            key={t}
            role="tab"
            aria-selected={tab === t}
            className={`px-3 py-1.5 text-sm rounded-lg ${tab === t ? "bg-sky-600 text-white" : "bg-white dark:bg-slate-800 border border-slate-300 dark:border-slate-600"}`}
            onClick={() => setTab(t)}
          >
            {t}
          </button>
        ))}
      </div>
      {tab === "Realms" && <RealmsTab />}
      {tab === "Tenants & projects" && <TenantsTab />}
      {tab === "Directories" && <DirectoriesTab />}
    </>
  );
}
