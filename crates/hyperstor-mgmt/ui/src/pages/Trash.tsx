// The trash (D54): deleted volumes and snapshots, kept for their pool's
// grace period. Operators restore (D114); storage administrators purge at
// once and set each pool's grace period.
import { useState } from "react";
import { cluster, trash } from "../api";
import type { TrashItem } from "../gen/hyperstor/v1/cluster_pb";
import { ActionDialog, Button, Card, Cli, ErrorText, Field, Pill, Table, fmtBytes, fmtTime, input, td, useCan, usePoll } from "../ui";

type Dialog = { kind: "restore" | "purge"; item: TrashItem } | { kind: "settings"; pool: number };

const what = (i: TrashItem) => (i.kind === "snapshot" ? `snapshot ${String(i.epoch)} of ${i.id}` : `volume ${i.name || i.id}`);
const cliOf = (i: TrashItem) => `--id ${i.id}${i.kind === "snapshot" ? ` --epoch ${String(i.epoch)}` : ""}`;

function Settings({ pool }: { pool: number }) {
  const s = usePoll(async () => (await trash.getTrashSettings({ pool })).settings, 0);
  const can = useCan();
  const [open, setOpen] = useState(false);
  const [grace, setGrace] = useState("");
  const [onFull, setOnFull] = useState("keep");
  const hours = s.data ? Number(s.data.graceSeconds) / 3600 : null;
  return (
    <div className="flex items-center gap-3 text-sm" data-testid={`trash-settings-${pool}`}>
      <span>
        Pool {pool}: kept {hours == null ? "…" : `${hours % 1 ? hours.toFixed(2) : hours} h`}; a full pool would{" "}
        {s.data?.onFull ?? "…"} it
      </span>
      {can("store") && (
        <Button
          onClick={() => {
            setGrace(String(Number(s.data?.graceSeconds ?? 86400n)));
            setOnFull(s.data?.onFull || "keep");
            setOpen(true);
          }}
        >
          Change
        </Button>
      )}
      {open && (
        <ActionDialog
          title={`Pool ${pool}'s trash`}
          submit="Save"
          cli={`trash settings --pool ${pool} --grace ${grace || "S"} --on-full ${onFull}`}
          run={() => trash.setTrashSettings({ pool, graceSeconds: BigInt(grace || "0"), onFull })}
          onClose={(done) => {
            setOpen(false);
            if (done) s.reload();
          }}
        >
          <Field label="Grace period (seconds; 0 purges at once)">
            <input className={input} value={grace} onChange={(e) => setGrace(e.target.value.replace(/\D/g, ""))} />
          </Field>
          <Field label="When the pool is full">
            <select className={input} value={onFull} onChange={(e) => setOnFull(e.target.value)}>
              <option value="keep">keep the trash (writes may fail)</option>
              <option value="purge">purge the oldest first</option>
            </select>
          </Field>
          <p className="text-xs text-slate-500">
            This needs a login of the last 10 minutes.
          </p>
        </ActionDialog>
      )}
    </div>
  );
}

export default function Trash() {
  const can = useCan();
  const items = usePoll(async () => (await trash.listTrash({ pool: 0 })).items, 5000);
  const status = usePoll(() => cluster.getStatus({}), 0);
  const pools = (status.data?.pools ?? []).filter((p) => p.id !== 0);
  const [dialog, setDialog] = useState<Dialog | null>(null);
  const [name, setName] = useState("");
  const [notice, setNotice] = useState("");
  const close = (done: boolean) => {
    setDialog(null);
    setName("");
    if (done) items.reload();
  };
  return (
    <>
      <Card title="Trash">
        <ErrorText error={items.error} />
        {notice && <p className="text-sm text-emerald-700 mb-2">{notice}</p>}
        <Table head={["What", "Volume", "Name", "Pool", "Size", "Deleted", "Purged", ""]} empty="The trash is empty.">
          {(items.data ?? []).map((i) => (
            <tr key={`${i.id}-${String(i.epoch)}`} data-testid={`trash-${i.name}`}>
              <td className={td}>
                <Pill tone="muted">{i.kind === "snapshot" ? `snapshot ${String(i.epoch)}` : "volume"}</Pill>
              </td>
              <td className={`${td} font-mono text-xs`}>{i.id}</td>
              <td className={td}>{i.name || "–"}</td>
              <td className={td}>{i.pool}</td>
              <td className={td}>{fmtBytes(i.size)}</td>
              <td className={td}>{fmtTime(i.trashedUnix)}</td>
              <td className={td}>{fmtTime(i.purgeUnix)}</td>
              <td className={`${td} flex gap-2`}>
                {can("operate") && <Button onClick={() => setDialog({ kind: "restore", item: i })}>Restore</Button>}
                {can("store") && (
                  <Button kind="danger" onClick={() => setDialog({ kind: "purge", item: i })}>
                    Purge now
                  </Button>
                )}
              </td>
            </tr>
          ))}
        </Table>
        <Cli cmd="trash list" />
      </Card>
      <Card title="Grace periods">
        <div className="grid gap-2">
          {pools.map((p) => (
            <Settings key={p.id} pool={p.id} />
          ))}
        </div>
      </Card>
      {dialog?.kind === "restore" && (
        <ActionDialog
          title={`Restore ${what(dialog.item)}`}
          submit="Restore"
          cli={`trash restore ${cliOf(dialog.item)}${name ? ` --name ${name}` : ""}`}
          run={() =>
            trash.restoreFromTrash({
              id: dialog.item.id,
              epoch: dialog.item.kind === "snapshot" ? dialog.item.epoch : undefined,
              name,
            })
          }
          onClose={close}
        >
          {dialog.item.kind === "volume" && (
            <Field label="Name (empty: its own, or NAME-restored if another volume took it)">
              <input className={input} value={name} onChange={(e) => setName(e.target.value)} />
            </Field>
          )}
        </ActionDialog>
      )}
      {dialog?.kind === "purge" && (
        <ActionDialog
          title={`Purge ${what(dialog.item)}`}
          submit="Purge now"
          kind="danger"
          cli={`trash purge ${cliOf(dialog.item)}`}
          run={async () => {
            const r = await trash.purgeFromTrash({
              id: dialog.item.id,
              epoch: dialog.item.kind === "snapshot" ? dialog.item.epoch : undefined,
            });
            setNotice(`Purging ${what(dialog.item)}: operation ${r.operation?.id} (Operations).`);
          }}
          onClose={close}
        >
          <p className="text-sm">
            Its data is deleted for good, as an operation (Operations). This needs a login of the last 10 minutes.
          </p>
        </ActionDialog>
      )}
    </>
  );
}
