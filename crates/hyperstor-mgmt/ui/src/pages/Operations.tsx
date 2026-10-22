// Running and recent operations with their progress; cancel (operators).
import { useState } from "react";
import { ops } from "../api";
import { OperationState, type Operation } from "../gen/hyperstor/v1/admin_pb";
import { ActionDialog, Card, Cli, ErrorText, Pill, Table, fmtTime, td, useCan, usePoll, Button } from "../ui";

const STATE: Record<number, [string, "ok" | "warn" | "error" | "muted"]> = {
  [OperationState.RUNNING]: ["running", "warn"],
  [OperationState.DONE]: ["done", "ok"],
  [OperationState.FAILED]: ["failed", "error"],
  [OperationState.CANCELLED]: ["cancelled", "muted"],
};

export default function Operations() {
  const can = useCan();
  const list = usePoll(async () => (await ops.listOperations({})).operations, 2000);
  const [cancel, setCancel] = useState<Operation | null>(null);
  return (
    <Card title="Operations">
      <ErrorText error={list.error} />
      <Table head={["Id", "Kind", "Target", "State", "Progress", "Started", "Finished", "By", "Message", ""]} empty="No operations.">
        {(list.data ?? []).map((o) => {
          const [text, tone] = STATE[o.state] ?? ["unknown", "muted"];
          return (
            <tr key={o.id} data-testid={`op-${o.id}`}>
              <td className={`${td} font-mono text-xs`}>{o.id}</td>
              <td className={td}>{o.kind}</td>
              <td className={td}>{o.target}</td>
              <td className={td}>
                <Pill tone={tone}>{text}</Pill>
              </td>
              <td className={td}>
                {o.total ? (
                  <div className="flex items-center gap-2">
                    <div className="w-24 h-2 rounded bg-slate-200 dark:bg-slate-700 overflow-hidden">
                      <div className="h-full bg-sky-500" style={{ width: `${(100 * o.done) / o.total}%` }} />
                    </div>
                    {o.done}/{o.total}
                  </div>
                ) : (
                  "–"
                )}
              </td>
              <td className={td}>{fmtTime(o.startedUnix)}</td>
              <td className={td}>{fmtTime(o.finishedUnix)}</td>
              <td className={`${td} text-xs`}>{o.createdBy}</td>
              <td className={`${td} whitespace-normal`}>{o.message}</td>
              <td className={td}>
                {can("operate") && o.state === OperationState.RUNNING && <Button onClick={() => setCancel(o)}>Cancel</Button>}
              </td>
            </tr>
          );
        })}
      </Table>
      <Cli cmd="op list" />
      {cancel && (
        <ActionDialog
          title={`Cancel ${cancel.kind} ${cancel.id}`}
          submit="Cancel it"
          kind="danger"
          cli={`op cancel --id ${cancel.id}`}
          run={() => ops.cancelOperation({ id: cancel.id })}
          onClose={(done) => {
            setCancel(null);
            if (done) list.reload();
          }}
        >
          <p className="text-sm">A cancelled drain brings the node's devices back in.</p>
        </ActionDialog>
      )}
    </Card>
  );
}
