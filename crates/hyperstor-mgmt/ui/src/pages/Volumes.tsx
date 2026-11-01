// A pool's volumes, and a volume's snapshots: create, expand and delete
// volumes, take snapshots (alone or as a consistency group), clone, roll back and
// flatten (storage administrators).
import { Fragment, useEffect, useState } from "react";
import { cluster } from "../api";
import type { Snapshot, Volume } from "../gen/hyperstor/v1/cluster_pb";
import { ActionDialog, Button, Card, Cli, ErrorText, Field, Pill, Table, fmtBytes, fmtTime, input, parseSize, td, useCan, usePoll } from "../ui";

type Dialog =
  | { kind: "create" }
  | { kind: "delete"; v: Volume }
  | { kind: "snapshot"; v: Volume }
  | { kind: "group"; ids: string[] }
  | { kind: "deleteSnap"; v: Volume; s: Snapshot }
  | { kind: "deleteGroup"; group: string }
  | { kind: "clone"; v: Volume; s: Snapshot }
  | { kind: "rollback"; v: Volume; s: Snapshot }
  | { kind: "flatten"; v: Volume }
  | { kind: "expand"; v: Volume };

function Snapshots({ v, open }: { v: Volume; open: (d: Dialog) => void }) {
  const can = useCan();
  const snaps = usePoll(async () => (await cluster.listSnapshots({ id: v.id })).snapshots, 5000);
  return (
    <div className="pl-4 py-2 border-l-2 border-sky-200 dark:border-sky-800">
      <ErrorText error={snaps.error} />
      <Table head={["Epoch", "Name", "Group", "State", ""]} empty="No snapshots.">
        {(snaps.data ?? []).map((s) => (
          <tr key={String(s.epoch)} data-testid={`snapshot-${s.name}`}>
            <td className={td}>{String(s.epoch)}</td>
            <td className={td}>{s.name || "–"}</td>
            <td className={`${td} font-mono text-xs`}>{s.group || "–"}</td>
            <td className={td}>{s.state}</td>
            <td className={`${td} flex gap-2`}>
              {can("store") && (
                <>
                  <Button onClick={() => open({ kind: "clone", v, s })}>Clone</Button>
                  <Button onClick={() => open({ kind: "rollback", v, s })}>Roll back</Button>
                  <Button kind="danger" onClick={() => open({ kind: "deleteSnap", v, s })}>
                    Delete
                  </Button>
                  {s.group && <Button onClick={() => open({ kind: "deleteGroup", group: s.group })}>Delete group</Button>}
                </>
              )}
            </td>
          </tr>
        ))}
      </Table>
      <Cli cmd={`snapshot list --id ${v.id}`} />
    </div>
  );
}

export default function Volumes() {
  const can = useCan();
  const status = usePoll(() => cluster.getStatus({}), 0);
  const pools = (status.data?.pools ?? []).filter((p) => p.id !== 0);
  const [pool, setPool] = useState<number | null>(null);
  useEffect(() => {
    if (pool == null && pools.length) setPool(pools[0].id);
  }, [pool, pools]);
  const vols = usePoll(async () => (pool == null ? [] : (await cluster.listVolumes({ pool })).volumes), 5000);
  useEffect(() => {
    vols.reload();
  }, [pool, vols.reload]);
  const [open, setOpen] = useState<string | null>(null);
  const [picked, setPicked] = useState<string[]>([]);
  const [dialog, setDialog] = useState<Dialog | null>(null);
  const [notice, setNotice] = useState("");
  // The forms' fields.
  const [name, setName] = useState("");
  const [size, setSize] = useState("10G");
  const [force, setForce] = useState(false);

  const close = (done: boolean) => {
    setDialog(null);
    setName("");
    setForce(false);
    if (done) vols.reload();
  };
  const show = (d: Dialog) => {
    setNotice("");
    setDialog(d);
  };

  return (
    <Card
      title="Volumes"
      actions={
        <>
          <select className={input} value={pool ?? ""} onChange={(e) => setPool(Number(e.target.value))} aria-label="Pool">
            {pools.map((p) => (
              <option key={p.id} value={p.id}>
                pool {p.id} {p.name}
              </option>
            ))}
          </select>
          {can("operate") && picked.length > 1 && <Button onClick={() => show({ kind: "group", ids: picked })}>Snapshot together</Button>}
          {can("store") && (
            <Button kind="primary" onClick={() => show({ kind: "create" })}>
              Create volume
            </Button>
          )}
        </>
      }
    >
      <ErrorText error={status.error ?? vols.error} />
      {notice && <p className="text-sm text-emerald-700 mb-2">{notice}</p>}
      <Table head={["", "Id", "Name", "Size", "State", "Attached by", ""]} empty="No volumes in this pool.">
        {(vols.data ?? []).map((v) => (
          <Fragment key={v.id}>
            <tr data-testid={`volume-${v.name}`}>
              <td className={td}>
                <input
                  type="checkbox"
                  aria-label={`pick ${v.name}`}
                  checked={picked.includes(v.id)}
                  onChange={(e) => setPicked((p) => (e.target.checked ? [...p, v.id] : p.filter((x) => x !== v.id)))}
                />
              </td>
              <td className={`${td} font-mono text-xs`}>{v.id}</td>
              <td className={td}>{v.name || "–"}</td>
              <td className={td}>{fmtBytes(v.size)}</td>
              <td className={td}>
                <Pill tone={v.state === "active" ? "ok" : "muted"}>{v.state}</Pill>
              </td>
              <td className={`${td} text-xs`}>{v.attachedBy || "–"}</td>
              <td className={`${td} flex gap-2`}>
                <Button onClick={() => setOpen(open === v.id ? null : v.id)}>{open === v.id ? "Hide snapshots" : "Snapshots"}</Button>
                {can("operate") && <Button onClick={() => show({ kind: "snapshot", v })}>Snapshot</Button>}
                {can("store") && (
                  <>
                    <Button
                      onClick={() => {
                        // Twice its size, in MiB, as a start.
                        setSize(`${(v.size * 2n) >> 20n}M`);
                        show({ kind: "expand", v });
                      }}
                    >
                      Expand
                    </Button>
                    <Button onClick={() => show({ kind: "flatten", v })}>Flatten</Button>
                    <Button kind="danger" onClick={() => show({ kind: "delete", v })}>
                      Delete
                    </Button>
                  </>
                )}
              </td>
            </tr>
            {open === v.id && (
              <tr>
                <td colSpan={7}>
                  <Snapshots v={v} open={show} />
                </td>
              </tr>
            )}
          </Fragment>
        ))}
      </Table>
      <Cli cmd={`volumes --pool ${pool ?? 1}`} />

      {dialog?.kind === "create" && pool != null && (
        <ActionDialog
          title="Create a volume"
          submit="Create"
          cli={`create-volume --pool ${pool} --size ${size || "S"}${name ? ` --name ${name}` : ""}`}
          run={async () => {
            const bytes = parseSize(size);
            if (bytes == null) throw new Error("size: bytes, or a number with K, M, G or T");
            const r = await cluster.createVolume({ pool, size: bytes, name });
            setNotice(`Created volume ${r.volume?.id}.`);
          }}
          onClose={close}
        >
          <Field label="Name">
            <input className={input} value={name} onChange={(e) => setName(e.target.value)} autoFocus />
          </Field>
          <Field label="Size (bytes, or with K, M, G or T)">
            <input className={input} value={size} onChange={(e) => setSize(e.target.value)} />
          </Field>
        </ActionDialog>
      )}
      {dialog?.kind === "delete" && (
        <ActionDialog
          title={`Delete volume ${dialog.v.name || dialog.v.id}`}
          submit="Move to the trash"
          kind="danger"
          cli={`delete-volume --id ${dialog.v.id}${force ? " --force" : ""}`}
          run={async () => {
            const r = await cluster.deleteVolume({ id: dialog.v.id, force });
            setNotice(`Moved to the trash: purged ${fmtTime(r.purgeUnix)} unless restored (Trash).`);
          }}
          onClose={close}
        >
          <p className="text-sm">
            It goes to the trash with its snapshots, and stays there for the pool's grace period (a day by default):
            restore it from the Trash page until then. This needs a login of the last 10 minutes.
          </p>
          {dialog.v.attachedBy && (
            <label className="text-sm flex gap-2 items-center">
              <input type="checkbox" checked={force} onChange={(e) => setForce(e.target.checked)} />
              Delete it although {dialog.v.attachedBy} has it attached (that client is blocklisted first)
            </label>
          )}
        </ActionDialog>
      )}
      {dialog?.kind === "snapshot" && (
        <ActionDialog
          title={`Snapshot of ${dialog.v.name || dialog.v.id}`}
          submit="Take snapshot"
          cli={`snapshot create --id ${dialog.v.id}${name ? ` --name ${name}` : ""}`}
          run={async () => {
            const r = await cluster.createSnapshot({ id: dialog.v.id, name });
            setNotice(`Snapshot at epoch ${String(r.snapshot?.epoch)}.`);
            setOpen(dialog.v.id);
          }}
          onClose={close}
        >
          <Field label="Name">
            <input className={input} value={name} onChange={(e) => setName(e.target.value)} autoFocus />
          </Field>
          <p className="text-xs text-slate-500">An attached volume's client is paused for it, at most 2 s.</p>
        </ActionDialog>
      )}
      {dialog?.kind === "group" && (
        <ActionDialog
          title={`Snapshot ${dialog.ids.length} volumes together`}
          submit="Take snapshots"
          cli={`snapshot group --ids ${dialog.ids.join(",")}${name ? ` --name ${name}` : ""}`}
          run={async () => {
            const r = await cluster.createGroupSnapshot({ ids: dialog.ids, name });
            setNotice(`Group ${r.group}: ${r.members.length} snapshots at one moment.`);
            setPicked([]);
          }}
          onClose={close}
        >
          <Field label="Name">
            <input className={input} value={name} onChange={(e) => setName(e.target.value)} autoFocus />
          </Field>
          <p className="text-xs text-slate-500">A consistency group: every volume's snapshot is of the same moment.</p>
        </ActionDialog>
      )}
      {dialog?.kind === "deleteSnap" && (
        <ActionDialog
          title={`Delete snapshot ${dialog.s.name || String(dialog.s.epoch)}`}
          submit="Move to the trash"
          kind="danger"
          cli={`snapshot delete --id ${dialog.v.id} --epoch ${String(dialog.s.epoch)}`}
          run={() => cluster.deleteSnapshot({ id: dialog.v.id, epoch: dialog.s.epoch })}
          onClose={close}
        >
          <p className="text-sm">It goes to the trash until the pool's grace period ends. Refused while a clone or a rollback depends on it.</p>
        </ActionDialog>
      )}
      {dialog?.kind === "deleteGroup" && (
        <ActionDialog
          title={`Delete group ${dialog.group}`}
          submit="Move to the trash"
          kind="danger"
          cli={`snapshot group-delete --group ${dialog.group}`}
          run={() => cluster.deleteGroupSnapshot({ group: dialog.group })}
          onClose={close}
        >
          <p className="text-sm">Every snapshot of the group goes to the trash.</p>
        </ActionDialog>
      )}
      {dialog?.kind === "clone" && (
        <ActionDialog
          title={`Clone ${dialog.v.name || dialog.v.id} at epoch ${String(dialog.s.epoch)}`}
          submit="Clone"
          cli={`clone-volume --id ${dialog.v.id} --epoch ${String(dialog.s.epoch)}${name ? ` --name ${name}` : ""}`}
          run={async () => {
            const r = await cluster.cloneVolume({ id: dialog.v.id, epoch: dialog.s.epoch, name });
            setNotice(`Clone ${r.volume?.id}.`);
          }}
          onClose={close}
        >
          <Field label="The clone's name">
            <input className={input} value={name} onChange={(e) => setName(e.target.value)} autoFocus />
          </Field>
          <p className="text-xs text-slate-500">It shares the snapshot's data until written; flatten it to stand alone.</p>
        </ActionDialog>
      )}
      {dialog?.kind === "rollback" && (
        <ActionDialog
          title={`Roll ${dialog.v.name || dialog.v.id} back`}
          submit="Roll back"
          kind="danger"
          cli={`rollback-volume --id ${dialog.v.id} --epoch ${String(dialog.s.epoch)}`}
          run={() => cluster.rollbackVolume({ id: dialog.v.id, epoch: dialog.s.epoch })}
          onClose={close}
        >
          <p className="text-sm">
            The volume becomes what it was at epoch {String(dialog.s.epoch)}; what was written since is lost. It must be
            detached. This needs a login of the last 10 minutes.
          </p>
        </ActionDialog>
      )}
      {dialog?.kind === "expand" && (
        <ActionDialog
          title={`Expand ${dialog.v.name || dialog.v.id}`}
          submit="Expand"
          cli={`expand-volume --id ${dialog.v.id} --size ${size}`}
          run={async () => {
            const bytes = parseSize(size);
            if (bytes == null) throw new Error("A size is like 20G, 512M or a number of bytes");
            if (bytes <= dialog.v.size) throw new Error(`A volume only grows: it is ${fmtBytes(dialog.v.size)}`);
            await cluster.expandVolume({ id: dialog.v.id, size: bytes });
            setNotice(`Expanded to ${fmtBytes(bytes)}.`);
          }}
          onClose={close}
        >
          <p className="text-sm">It is {fmtBytes(dialog.v.size)}; a volume only grows. A client that has it attached sees the new size once it attaches again.</p>
          <Field label="New size (G, M or bytes)">
            <input className={input} value={size} onChange={(e) => setSize(e.target.value)} />
          </Field>
        </ActionDialog>
      )}
      {dialog?.kind === "flatten" && (
        <ActionDialog
          title={`Flatten ${dialog.v.name || dialog.v.id}`}
          submit="Flatten"
          cli={`flatten-volume --id ${dialog.v.id}`}
          run={async () => {
            const r = await cluster.flattenVolume({ id: dialog.v.id });
            setNotice(`Flattening: operation ${r.operation?.id} (Operations).`);
          }}
          onClose={close}
        >
          <p className="text-sm">
            Copies in what a clone reads from its parents, so they can be deleted. It runs as an operation, followed on
            the Operations page.
          </p>
        </ActionDialog>
      )}
    </Card>
  );
}
