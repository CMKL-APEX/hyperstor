// The map service's members (D87): voters and learners, who leads; add
// and remove (storage administrators). A new member must be running,
// joined with a meta token, before it is added.
import { useState } from "react";
import { members } from "../api";
import type { Member } from "../gen/hyperstor/v1/admin_pb";
import { ActionDialog, Button, Card, Cli, ErrorText, Field, Pill, Table, input, td, useCan, usePoll } from "../ui";

export default function Members() {
  const can = useCan();
  const list = usePoll(() => members.listMembers({}), 3000);
  const [add, setAdd] = useState(false);
  const [remove, setRemove] = useState<Member | null>(null);
  const [id, setId] = useState("");
  const [raft, setRaft] = useState("");
  const [map, setMap] = useState("");
  const [force, setForce] = useState(false);
  const voters = (list.data?.members ?? []).filter((m) => m.voter).length;
  return (
    <Card
      title="Map service members"
      actions={
        can("store") && (
          <Button kind="primary" onClick={() => setAdd(true)}>
            Add member
          </Button>
        )
      }
    >
      <ErrorText error={list.error} />
      <Table head={["Id", "Raft", "Map", "Kind", "", ""]}>
        {(list.data?.members ?? []).map((m) => (
          <tr key={String(m.id)}>
            <td className={td}>{String(m.id)}</td>
            <td className={td}>{m.raftAddress}</td>
            <td className={td}>{m.mapAddress}</td>
            <td className={td}>
              <Pill tone={m.voter ? "ok" : "muted"}>{m.voter ? "voter" : "learner"}</Pill>
            </td>
            <td className={td}>{m.leader && <Pill tone="ok">leader</Pill>}</td>
            <td className={td}>
              {can("store") && (
                <Button kind="danger" onClick={() => setRemove(m)}>
                  Remove
                </Button>
              )}
            </td>
          </tr>
        ))}
      </Table>
      <Cli cmd="member list" />
      {add && (
        <ActionDialog
          title="Add a map service member"
          submit="Add"
          cli={`member add --id ${id || "N"} --raft ${raft || "ADDR"} --map ${map || "ADDR"}`}
          run={() => members.addMember({ id: BigInt(id || "0"), raftAddress: raft, mapAddress: map })}
          onClose={(done) => {
            setAdd(false);
            if (done) list.reload();
          }}
        >
          <Field label="Member id">
            <input className={input} value={id} onChange={(e) => setId(e.target.value.replace(/\D/g, ""))} />
          </Field>
          <Field label="Raft address (IP:PORT)">
            <input className={input} value={raft} onChange={(e) => setRaft(e.target.value)} />
          </Field>
          <Field label="Map address (IP:PORT)">
            <input className={input} value={map} onChange={(e) => setMap(e.target.value)} />
          </Field>
          <p className="text-xs text-slate-500">A learner until it has caught up (up to two minutes), then a voter.</p>
        </ActionDialog>
      )}
      {remove && (
        <ActionDialog
          title={`Remove member ${String(remove.id)}`}
          submit="Remove"
          kind="danger"
          cli={`member remove --id ${String(remove.id)}${force ? " --force" : ""}`}
          run={() => members.removeMember({ id: remove.id, force })}
          onClose={(done) => {
            setRemove(null);
            setForce(false);
            if (done) list.reload();
          }}
        >
          <p className="text-sm">This needs a login of the last 10 minutes.</p>
          {remove.voter && voters <= 3 && (
            <label className="text-sm flex gap-2 items-center">
              <input type="checkbox" checked={force} onChange={(e) => setForce(e.target.checked)} />
              Remove it although fewer than three voters would be left
            </label>
          )}
        </ActionDialog>
      )}
    </Card>
  );
}
