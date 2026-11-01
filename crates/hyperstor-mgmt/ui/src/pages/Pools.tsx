// Pools with their PG states, and one pool's PGs as a grid (D154): a
// PG's acting and up sets when it is picked, the whole table on request.
// Storage administrators create pools, change them (classes, name,
// domain, a split, autoscale, a cache pool's grace), choose the devices
// they use beyond their classes, and delete them.
import { useEffect, useState } from "react";
import { cluster } from "../api";
import type { Device, Pool, Volume } from "../gen/hyperstor/v1/cluster_pb";
import { PgGrid } from "../PgGrid";
import { ActionDialog, Button, Card, Cli, ErrorText, Field, Space, Table, input, td, useCan, usePoll } from "../ui";

type Dialog =
  | { kind: "create" }
  | { kind: "edit"; p: Pool }
  | { kind: "devices"; p: Pool }
  | { kind: "delete"; p: Pool; volumes: Volume[] };

/** Whether pool `p` uses device `d` (D180): of its classes (none: every
 * device), less those removed from it, plus those added. */
const uses = (p: Pool, d: Device) =>
  p.addedDevices.includes(d.id) ||
  ((p.classes.length === 0 || p.classes.includes(d.class)) && !p.removedDevices.includes(d.id));

const splitClasses = (s: string) =>
  s
    .split(",")
    .map((c) => c.trim())
    .filter(Boolean);

const STATES = [
  ["clean", "bg-emerald-500"],
  ["degraded", "bg-amber-500"],
  ["backfilling", "bg-sky-500"],
  ["inactive", "bg-red-500"],
] as const;

const NONE = 4294967295;
const set = (v: number[]) => `[${v.map((d) => (d === NONE ? "-" : d)).join(",")}]`;

/** The pool's devices, each used or not, with a button to change it. */
function PoolDevices({ p, devices, onChange }: { p: Pool; devices: Device[]; onChange: () => void }) {
  const [error, setError] = useState<unknown>();
  const [force, setForce] = useState(false);
  const toggle = async (d: Device) => {
    setError(undefined);
    try {
      await cluster.setPoolDevice({ pool: p.id, device: d.id, member: !uses(p, d), force });
      onChange();
    } catch (e) {
      setError(e);
    }
  };
  return (
    <div className="grid gap-2">
      <Table head={["Device", "Class", "Used", ""]}>
        {devices.map((d) => (
          <tr key={d.id} data-testid={`pool-device-${d.id}`}>
            <td className={td}>{d.id}</td>
            <td className={td}>{d.class || "–"}</td>
            <td className={td}>
              {uses(p, d) ? "yes" : "no"}
              {p.addedDevices.includes(d.id) && " (added)"}
              {p.removedDevices.includes(d.id) && " (removed)"}
            </td>
            <td className={td}>
              <Button onClick={() => toggle(d)}>{uses(p, d) ? "Stop using" : "Use"}</Button>
            </td>
          </tr>
        ))}
      </Table>
      <label className="text-sm flex gap-2 items-center">
        <input type="checkbox" checked={force} onChange={(e) => setForce(e.target.checked)} />
        Allow hard disks with flash (force)
      </label>
      <ErrorText error={error} />
      <p className="text-sm">Its PGs move as its devices change (D180).</p>
      <Cli cmd={`pool add-device --pool ${p.id} --device D`} />
    </div>
  );
}

export default function Pools() {
  const can = useCan();
  const status = usePoll(() => cluster.getStatus({}), 2000);
  const [dialog, setDialog] = useState<Dialog | null>(null);
  const [error, setError] = useState<unknown>();
  const [notice, setNotice] = useState("");
  // The forms' fields.
  const [name, setName] = useState("");
  const [kind, setKind] = useState("replicated");
  const [classes, setClasses] = useState("");
  const [copies, setCopies] = useState("");
  const [min, setMin] = useState("");
  const [pgCount, setPgCount] = useState("");
  const [domain, setDomain] = useState("host");
  const [autoscale, setAutoscale] = useState(true);
  const [grace, setGrace] = useState("60");
  const [split, setSplit] = useState(false);
  const [force, setForce] = useState(false);
  const devices = (status.data?.nodes ?? []).flatMap((n) => n.devices);
  const close = (done: boolean) => {
    setDialog(null);
    setForce(false);
    setSplit(false);
    if (done) status.reload();
  };
  const create = () => {
    setName("");
    setKind("replicated");
    setClasses(Array.from(new Set(devices.map((d) => d.class).filter(Boolean))).join(","));
    setCopies("");
    setMin("");
    setPgCount("");
    setDomain("host");
    setAutoscale(true);
    setGrace("60");
    setDialog({ kind: "create" });
  };
  const edit = (p: Pool) => {
    setName(p.name);
    setClasses(p.classes.length ? p.classes.join(",") : "any");
    setDomain(p.domain);
    setAutoscale(p.autoscale);
    setGrace(String(p.cacheGrace));
    setDialog({ kind: "edit", p });
  };
  // What a delete would take with it, first (D176).
  const planDelete = async (p: Pool) => {
    setError(undefined);
    try {
      const r = await cluster.deletePool({ pool: p.id, dryRun: true });
      setDialog({ kind: "delete", p, volumes: r.volumes });
    } catch (e) {
      setError(e);
    }
  };
  const num = (s: string, what: string) => {
    if (s.trim() === "") return undefined;
    const n = Number(s);
    if (!Number.isInteger(n) || n < 0) throw new Error(`${what} is a whole number`);
    return n;
  };
  const createCli = () =>
    [
      `pool create --name ${name || "N"} --classes ${classes || "C"}`,
      kind !== "replicated" && `--kind ${kind}`,
      copies && `--copies ${copies}`,
      min && `--min ${min}`,
      pgCount && `--pgs ${pgCount}`,
      domain !== "host" && `--domain ${domain}`,
      !autoscale && "--autoscale off",
      kind === "cache" && grace !== "60" && `--cache-grace ${grace}`,
      force && "--force",
    ]
      .filter(Boolean)
      .join(" ");
  const editCli = (p: Pool) => {
    const was = p.classes.length ? p.classes.join(",") : "any";
    return [
      `pool set --pool ${p.id}`,
      classes !== was && `--classes ${classes}`,
      name !== p.name && `--name ${name}`,
      domain !== p.domain && `--domain ${domain}`,
      split && `--pgs ${p.pgCount * 2}`,
      autoscale !== p.autoscale && `--autoscale ${autoscale ? "on" : "off"}`,
      p.kind === "cache" && grace !== String(p.cacheGrace) && `--cache-grace ${grace}`,
      force && "--force",
    ]
      .filter(Boolean)
      .join(" ");
  };
  const [pool, setPool] = useState<number | null>(null);
  const [pg, setPg] = useState<number | null>(null);
  const [all, setAll] = useState(false);
  // Loaded when asked for, not polled: a pool may have many PGs.
  const pgs = usePoll(
    async () => (pool == null || (pg == null && !all) ? null : (await cluster.listPgs({ pool, pg: all ? undefined : pg! })).pgs),
    0,
  );
  const { reload } = pgs;
  useEffect(() => {
    reload();
  }, [reload, pool, pg, all]);
  const open = (p: number) => {
    setPool(p);
    setPg(null);
    setAll(false);
  };
  return (
    <>
      <Card title="Pools" actions={can("store") && <Button onClick={create}>Create pool</Button>}>
        <ErrorText error={status.error ?? error} />
        {notice && <p className="text-sm text-emerald-700 mb-2">{notice}</p>}
        <Table head={["Pool", "Kind", "Copies", "Domain", "Chunk", "PGs", "Space", "States", "Clean", "Degraded", "Backfilling", "Inactive", ""]}>
          {(status.data?.pools ?? []).map((p) => {
            const c = p.pgs;
            const total = p.pgCount || 1;
            return (
              <tr key={p.id} data-testid={`pool-${p.id}`}>
                <td className={td}>
                  {p.id} {p.name}
                </td>
                <td className={td}>{p.kind}</td>
                <td className={td}>
                  {p.size}/{p.minSize}
                </td>
                <td className={td}>{p.domain}</td>
                <td className={td}>{p.chunkSize ? `${p.chunkSize / 1024}K` : "–"}</td>
                <td className={td}>{p.pgCount}</td>
                <td className={td}>
                  <Space total={p.totalBytes} free={p.freeBytes} testId={`pool-${p.id}-space`} />
                  {(p.fullness === "nearfull" || p.fullness === "full") && (
                    // Past a threshold (D27): full refuses new data.
                    <div
                      data-testid={`pool-${p.id}-fullness`}
                      className={`text-xs ${p.fullness === "full" ? "text-red-700 dark:text-red-400" : "text-amber-700 dark:text-amber-400"}`}
                    >
                      {p.fullness}
                      {p.fullestDevice != null && ` (device ${p.fullestDevice})`}
                    </div>
                  )}
                </td>
                <td className={td}>
                  <div className="flex h-2.5 w-40 rounded overflow-hidden bg-slate-200 dark:bg-slate-700">
                    {STATES.map(([k, color]) => {
                      const n = c?.[k] ?? 0;
                      return n ? <span key={k} className={color} style={{ width: `${(100 * n) / total}%` }} /> : null;
                    })}
                  </div>
                </td>
                <td className={td}>{c?.clean ?? 0}</td>
                <td className={td}>{c?.degraded ?? 0}</td>
                <td className={td}>{c?.backfilling ?? 0}</td>
                <td className={td}>{c?.inactive ?? 0}</td>
                <td className={`${td} flex gap-2 items-center`}>
                  <button className="text-sky-700 dark:text-sky-300 hover:underline" onClick={() => open(p.id)}>
                    PGs
                  </button>
                  {can("store") && p.id !== 0 && (
                    <>
                      <Button onClick={() => edit(p)}>Edit</Button>
                      <Button onClick={() => setDialog({ kind: "devices", p })}>Devices</Button>
                      <Button kind="danger" onClick={() => planDelete(p)}>
                        Delete
                      </Button>
                    </>
                  )}
                </td>
              </tr>
            );
          })}
        </Table>
        <Cli cmd="pools" />
      </Card>
      {dialog?.kind === "create" && (
        <ActionDialog
          title="Create a pool"
          submit="Create"
          cli={createCli()}
          run={async () => {
            const size = num(copies, "Copies");
            const r = await cluster.createPool({
              name: name.trim(),
              kind,
              size: size ?? (kind === "cache" ? 1 : 3),
              minSize: num(min, "Min"),
              pgCount: num(pgCount, "PGs"),
              domain,
              classes: splitClasses(classes),
              force,
              autoscale,
              cacheGrace: kind === "cache" ? num(grace, "The grace") : undefined,
            });
            setNotice(`Pool ${r.pool} ${name} made, ${r.pgCount} PGs.`);
          }}
          onClose={close}
        >
          <Field label="Name">
            <input className={input} value={name} onChange={(e) => setName(e.target.value)} />
          </Field>
          <Field label="Kind">
            <select className={input} value={kind} onChange={(e) => setKind(e.target.value)}>
              <option value="replicated">replicated</option>
              <option value="cache">cache</option>
            </select>
          </Field>
          <Field label="Classes (comma-separated)">
            <input className={input} value={classes} onChange={(e) => setClasses(e.target.value)} />
          </Field>
          <div className="grid grid-cols-3 gap-2">
            <Field label={`Copies (${kind === "cache" ? 1 : 3})`}>
              <input className={input} value={copies} onChange={(e) => setCopies(e.target.value)} />
            </Field>
            <Field label="Min (half, rounded up)">
              <input className={input} value={min} onChange={(e) => setMin(e.target.value)} />
            </Field>
            <Field label="PGs (about 100 per device)">
              <input className={input} value={pgCount} onChange={(e) => setPgCount(e.target.value)} />
            </Field>
          </div>
          <Field label="A copy per">
            <select className={input} value={domain} onChange={(e) => setDomain(e.target.value)}>
              <option value="device">device</option>
              <option value="host">host</option>
              <option value="rack">rack</option>
            </select>
          </Field>
          <label className="text-sm flex gap-2 items-center">
            <input type="checkbox" checked={autoscale} onChange={(e) => setAutoscale(e.target.checked)} />
            Double its PGs as devices are added (autoscale)
          </label>
          {kind === "cache" && (
            <Field label="Seconds a PG waits for a down node (0–3600)">
              <input className={input} value={grace} onChange={(e) => setGrace(e.target.value)} />
            </Field>
          )}
          <label className="text-sm flex gap-2 items-center">
            <input type="checkbox" checked={force} onChange={(e) => setForce(e.target.checked)} />
            Allow hard disks with flash (force)
          </label>
        </ActionDialog>
      )}
      {dialog?.kind === "edit" && (
        <ActionDialog
          title={`Change pool ${dialog.p.id} ${dialog.p.name}`}
          submit="Save"
          cli={editCli(dialog.p)}
          run={async () => {
            const p = dialog.p;
            const was = p.classes.length ? p.classes.join(",") : "any";
            const every = classes.trim() === "any";
            const r = await cluster.setPool({
              pool: p.id,
              classes: classes !== was && !every ? splitClasses(classes) : [],
              everyDevice: classes !== was && every,
              name: name !== p.name ? name.trim() : undefined,
              domain: domain !== p.domain ? domain : undefined,
              pgCount: split ? p.pgCount * 2 : undefined,
              autoscale: autoscale !== p.autoscale ? autoscale : undefined,
              cacheGrace: p.kind === "cache" && grace !== String(p.cacheGrace) ? num(grace, "The grace") : undefined,
              force,
            });
            setNotice(
              `Pool ${p.id} changed${split ? `; splitting to ${p.pgCount * 2} PGs` : ""}${r.mixedKinds ? " (hard disks with flash)" : ""}.`,
            );
          }}
          onClose={close}
        >
          <Field label="Name">
            <input className={input} value={name} onChange={(e) => setName(e.target.value)} />
          </Field>
          <Field label="Classes (comma-separated; any: every device)">
            <input className={input} value={classes} onChange={(e) => setClasses(e.target.value)} />
          </Field>
          <Field label="A copy per">
            <select className={input} value={domain} onChange={(e) => setDomain(e.target.value)}>
              <option value="device">device</option>
              <option value="host">host</option>
              <option value="rack">rack</option>
            </select>
          </Field>
          <label className="text-sm flex gap-2 items-center">
            <input type="checkbox" checked={autoscale} onChange={(e) => setAutoscale(e.target.checked)} />
            Double its PGs as devices are added (autoscale)
          </label>
          {!dialog.p.splitting && (
            <label className="text-sm flex gap-2 items-center">
              <input type="checkbox" checked={split} onChange={(e) => setSplit(e.target.checked)} />
              Split its {dialog.p.pgCount} PGs to {dialog.p.pgCount * 2}
            </label>
          )}
          {dialog.p.kind === "cache" && (
            <Field label="Seconds a PG waits for a down node (0–3600)">
              <input className={input} value={grace} onChange={(e) => setGrace(e.target.value)} />
            </Field>
          )}
          <label className="text-sm flex gap-2 items-center">
            <input type="checkbox" checked={force} onChange={(e) => setForce(e.target.checked)} />
            Allow hard disks with flash (force)
          </label>
          <p className="text-sm">PGs move as its classes or domain change; a split pauses each PG for a moment.</p>
        </ActionDialog>
      )}
      {dialog?.kind === "devices" && (
        <ActionDialog title={`Pool ${dialog.p.id} ${dialog.p.name}'s devices`} submit="Done" run={async () => {}} onClose={close}>
          <PoolDevices
            p={(status.data?.pools ?? []).find((q) => q.id === dialog.p.id) ?? dialog.p}
            devices={devices}
            onChange={status.reload}
          />
        </ActionDialog>
      )}
      {dialog?.kind === "delete" && (
        <ActionDialog
          title={`Delete pool ${dialog.p.id} ${dialog.p.name}`}
          submit={dialog.volumes.length ? "Delete it and its volumes" : "Delete"}
          kind="danger"
          cli={`pool delete --pool ${dialog.p.id}${dialog.volumes.length ? " --force" : ""}`}
          run={async () => {
            if (dialog.volumes.length && !force) throw new Error("Tick the box to delete its volumes too");
            const r = await cluster.deletePool({ pool: dialog.p.id, force: dialog.volumes.length > 0 });
            setNotice(
              r.operation?.id
                ? `Deleting pool ${dialog.p.id} with its volumes: operation ${r.operation.id} (Operations).`
                : `Pool ${dialog.p.id} deleted.`,
            );
          }}
          onClose={close}
        >
          <p className="text-sm">
            It leaves the map at once and its id is never given again; each device deletes its data within a minute or so
            (D181). This needs a login of the last 10 minutes.
          </p>
          {dialog.volumes.length > 0 && (
            <>
              <p className="text-sm text-red-700">
                It holds {dialog.volumes.length} volume(s), deleted first with their snapshots, even attached ones:
              </p>
              <ul className="text-xs font-mono max-h-40 overflow-auto">
                {dialog.volumes.map((v) => (
                  <li key={v.id}>
                    {v.name || v.id}
                    {v.attachedBy && ` (attached by ${v.attachedBy})`}
                  </li>
                ))}
              </ul>
              <label className="text-sm flex gap-2 items-center">
                <input type="checkbox" checked={force} onChange={(e) => setForce(e.target.checked)} />
                Delete them too (force)
              </label>
            </>
          )}
        </ActionDialog>
      )}
      {pool != null && (
        <Card
          title={`Pool ${pool}'s PGs`}
          actions={!all && <Button onClick={() => setAll(true)}>List all</Button>}
        >
          <PgGrid key={pool} pool={pool} max={16} picked={all ? null : pg} onPick={(i) => (setAll(false), setPg(i))} />
          <ErrorText error={pgs.error} />
          {(pg != null || all) && (
            <Table head={["PG", "Epoch", "Acting", "Up"]}>
              {(pgs.data ?? []).map((g) => (
                <tr key={g.pg}>
                  <td className={td}>
                    {pool}.{g.pg}
                  </td>
                  <td className={td}>{String(g.epoch)}</td>
                  <td className={`${td} font-mono`}>{set(g.acting)}</td>
                  <td className={`${td} font-mono`}>{set(g.up)}</td>
                </tr>
              ))}
            </Table>
          )}
          <Cli cmd={pg != null && !all ? `pgs --pool ${pool} --pg ${pg}` : `pg-grid --pool ${pool}`} />
        </Card>
      )}
    </>
  );
}
