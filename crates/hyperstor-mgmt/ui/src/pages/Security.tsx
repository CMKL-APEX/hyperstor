// People and their roles, your API tokens, join tokens and certificates
// (the map service's backup stays with hyperstorctl: a file to keep). Everyone manages their own API tokens; the
// rest is for security administrators.
import { useState } from "react";
import { Modal, TokenReveal, useSession } from "@glidex/ui-kit";
import { ROLES, ca, people, tokens, type Me, type Methods, type Person } from "../api";
import { ActionDialog, Button, Card, Cli, ErrorText, Field, Pill, Table, fmtTime, input, td, useCan, usePoll } from "../ui";

function People() {
  const { me, refresh } = useSession<Me, Methods>();
  const list = usePoll(people.list, 0);
  const [edit, setEdit] = useState<Person | null>(null);
  const [del, setDel] = useState<Person | null>(null);
  const [roles, setRoles] = useState<string[]>([]);
  return (
    <Card title="People">
      <ErrorText error={list.error} />
      <Table head={["Name", "Roles", "Since", ""]}>
        {(list.data ?? []).map((p) => (
          <tr key={p.name} data-testid={`person-${p.name}`}>
            <td className={td}>
              {p.name}
              {p.name === me.name && " (you)"}
            </td>
            <td className={td}>{p.roles.length ? p.roles.join(", ") : <span className="text-slate-500">none</span>}</td>
            <td className={td}>{fmtTime(p.created)}</td>
            <td className={`${td} flex gap-2`}>
              <Button
                onClick={() => {
                  setRoles(p.roles);
                  setEdit(p);
                }}
              >
                Roles
              </Button>
              <Button kind="danger" onClick={() => setDel(p)}>
                Delete
              </Button>
            </td>
          </tr>
        ))}
      </Table>
      <p className="mt-2 text-xs text-slate-500">
        People appear here when they first log in, with no roles. viewer reads; operator also takes devices out and
        in, drains and cancels; storage-admin also manages volumes, snapshots and members; security-admin manages
        people, tokens, certificates and backups, and nothing else.
      </p>
      {edit && (
        <ActionDialog
          title={`${edit.name}'s roles`}
          submit="Save"
          run={() => people.setRoles(edit.name, roles)}
          onClose={(done) => {
            const mine = edit.name === me.name;
            setEdit(null);
            if (done) list.reload();
            // Your own roles: what the pages show changes now.
            if (done && mine) refresh();
          }}
        >
          {ROLES.map((r) => (
            <label key={r} className="text-sm flex gap-2 items-center">
              <input
                type="checkbox"
                aria-label={r}
                checked={roles.includes(r)}
                onChange={(e) => setRoles((x) => (e.target.checked ? [...x, r] : x.filter((y) => y !== r)))}
              />
              {r}
            </label>
          ))}
          <p className="text-xs text-slate-500">This needs a login of the last 10 minutes.</p>
        </ActionDialog>
      )}
      {del && (
        <ActionDialog
          title={`Delete ${del.name}`}
          submit="Delete"
          kind="danger"
          run={() => people.remove(del.name)}
          onClose={(done) => {
            setDel(null);
            if (done) list.reload();
          }}
        >
          <p className="text-sm">Their sessions and API tokens end. A next login makes a new account with no roles.</p>
        </ActionDialog>
      )}
    </Card>
  );
}

function ApiTokens() {
  const list = usePoll(people.tokens, 0);
  const [create, setCreate] = useState(false);
  const [name, setName] = useState("");
  const [days, setDays] = useState("90");
  const [made, setMade] = useState<string | null>(null);
  const [revoke, setRevoke] = useState<string | null>(null);
  return (
    <Card
      title="Your API tokens"
      actions={
        <Button kind="primary" onClick={() => setCreate(true)}>
          Make a token
        </Button>
      }
    >
      <ErrorText error={list.error} />
      <Table head={["Id", "Name", "Made", "Expires", ""]} empty="No tokens.">
        {(list.data ?? []).map((t) => (
          <tr key={t.id}>
            <td className={`${td} font-mono text-xs`}>{t.id}</td>
            <td className={td}>{t.name}</td>
            <td className={td}>{fmtTime(t.created)}</td>
            <td className={td}>{fmtTime(t.expires)}</td>
            <td className={td}>
              <Button kind="danger" onClick={() => setRevoke(t.id)}>
                Revoke
              </Button>
            </td>
          </tr>
        ))}
      </Table>
      <p className="mt-2 text-xs text-slate-500">
        For scripts and <code>hyperstorctl --mgmt</code>: they act with your roles, but never for what needs a fresh
        login.
      </p>
      {create && (
        <ActionDialog
          title="Make an API token"
          submit="Make"
          run={async () => {
            const r = await people.createToken(name, Number(days));
            setMade(r.token);
          }}
          onClose={(done) => {
            setCreate(false);
            setName("");
            if (done) list.reload();
          }}
        >
          <Field label="Name (what it's for)">
            <input className={input} value={name} onChange={(e) => setName(e.target.value)} autoFocus />
          </Field>
          <Field label="Valid for (days, at most 365)">
            <input className={input} value={days} onChange={(e) => setDays(e.target.value.replace(/\D/g, ""))} />
          </Field>
          <p className="text-xs text-slate-500">This needs a login of the last 10 minutes.</p>
        </ActionDialog>
      )}
      {made && (
        <Modal title="Your new API token" onClose={() => setMade(null)}>
          <TokenReveal
            secret={made}
            warning="It is shown this once: copy it now and keep it where only you can read it."
            copiedHint="Copied. Use it as HYPERSTOR_TOKEN, or Authorization: Bearer."
          >
            <Cli cmd="--mgmt URL --token-file FILE status" />
          </TokenReveal>
        </Modal>
      )}
      {revoke && (
        <ActionDialog
          title="Revoke the token"
          submit="Revoke"
          kind="danger"
          run={() => people.revokeToken(revoke)}
          onClose={(done) => {
            setRevoke(null);
            if (done) list.reload();
          }}
        >
          <p className="text-sm">Whatever uses it stops working at once.</p>
        </ActionDialog>
      )}
    </Card>
  );
}

function JoinTokens() {
  const list = usePoll(async () => (await tokens.listTokens({})).tokens, 0);
  const [create, setCreate] = useState(false);
  const [role, setRole] = useState("node");
  const [ttl, setTtl] = useState("3600");
  const [made, setMade] = useState<{ token: string; api: string; role: string } | null>(null);
  const [revoke, setRevoke] = useState<string | null>(null);
  const now = Date.now() / 1000;
  return (
    <Card
      title="Join tokens"
      actions={
        <Button kind="primary" onClick={() => setCreate(true)}>
          Make a join token
        </Button>
      }
    >
      <ErrorText error={list.error} />
      <Table head={["Id", "Role", "Expires", "State", "Made by", ""]} empty="No join tokens.">
        {(list.data ?? []).map((t) => {
          const state = t.revoked ? "revoked" : t.used ? "used" : Number(t.expiresUnix) <= now ? "expired" : "valid";
          return (
            <tr key={t.id}>
              <td className={`${td} font-mono text-xs`}>{t.id}</td>
              <td className={td}>{t.role}</td>
              <td className={td}>{fmtTime(t.expiresUnix)}</td>
              <td className={td}>
                <Pill tone={state === "valid" ? "ok" : "muted"}>{state}</Pill>
              </td>
              <td className={`${td} text-xs`}>{t.createdBy}</td>
              <td className={td}>
                {state === "valid" && (
                  <Button kind="danger" onClick={() => setRevoke(t.id)}>
                    Revoke
                  </Button>
                )}
              </td>
            </tr>
          );
        })}
      </Table>
      <Cli cmd="token list" />
      {create && (
        <ActionDialog
          title="Make a join token"
          submit="Make"
          cli={`token create --role ${role} --ttl ${ttl}`}
          run={async () => {
            const r = await tokens.createToken({ role, ttlSeconds: Number(ttl) });
            setMade({ token: r.token, api: r.apiAddress, role });
          }}
          onClose={(done) => {
            setCreate(false);
            if (done) list.reload();
          }}
        >
          <Field label="For">
            <select className={input} value={role} onChange={(e) => setRole(e.target.value)}>
              <option value="node">a storage host (node)</option>
              <option value="meta">a map service member (meta)</option>
            </select>
          </Field>
          <Field label="Valid for (seconds, at most a week)">
            <input className={input} value={ttl} onChange={(e) => setTtl(e.target.value.replace(/\D/g, ""))} />
          </Field>
          <p className="text-xs text-slate-500">Single use. This needs a login of the last 10 minutes.</p>
        </ActionDialog>
      )}
      {made && (
        <Modal title="The join token" onClose={() => setMade(null)}>
          <TokenReveal
            secret={made.token}
            warning="It is shown this once, and lets one host join the cluster: pass it only to that host."
            copiedHint="Copied."
          >
            <p className="mt-2 text-xs text-slate-500">On the new host:</p>
            <code className="block text-xs p-2 rounded bg-slate-100 dark:bg-slate-800">
              sudo hyperstor-install install --join {made.api} --token-file FILE{made.role === "meta" ? " --role meta" : ""}
            </code>
          </TokenReveal>
        </Modal>
      )}
      {revoke && (
        <ActionDialog
          title="Revoke the join token"
          submit="Revoke"
          kind="danger"
          cli={`token revoke --id ${revoke}`}
          run={() => tokens.revokeToken({ id: revoke })}
          onClose={(done) => {
            setRevoke(null);
            if (done) list.reload();
          }}
        />
      )}
    </Card>
  );
}

function Certificates() {
  const list = usePoll(() => ca.listCertificates({}), 0);
  const [revoke, setRevoke] = useState<string | null>(null);
  return (
    <Card title="Certificates">
      <ErrorText error={list.error} />
      <Table head={["Serial", "Identity", "Issued", "Expires", "By", ""]} empty="None issued through the API.">
        {(list.data?.certificates ?? []).map((c) => (
          <tr key={c.serial}>
            <td className={`${td} font-mono text-xs`}>{c.serial}</td>
            <td className={`${td} text-xs`}>{c.identity}</td>
            <td className={td}>{fmtTime(c.issuedUnix)}</td>
            <td className={td}>{fmtTime(c.notAfterUnix)}</td>
            <td className={`${td} text-xs`}>{c.issuedBy}</td>
            <td className={td}>
              {c.revoked ? (
                <Pill tone="error">revoked</Pill>
              ) : (
                <Button kind="danger" onClick={() => setRevoke(c.serial)}>
                  Revoke
                </Button>
              )}
            </td>
          </tr>
        ))}
      </Table>
      <Cli cmd="cert list" />
      {revoke && (
        <ActionDialog
          title="Revoke the certificate"
          submit="Revoke"
          kind="danger"
          cli={`cert revoke --serial ${revoke}`}
          run={() => ca.revokeCertificate({ serial: revoke })}
          onClose={(done) => {
            setRevoke(null);
            if (done) list.reload();
          }}
        >
          <p className="text-sm">
            Every map service member refuses it from now on, until it would have expired. This needs a login of the
            last 10 minutes.
          </p>
        </ActionDialog>
      )}
    </Card>
  );
}

export default function Security() {
  const can = useCan();
  const tabs = [
    ...(can("secure") ? (["People", "Join tokens", "Certificates"] as const) : []),
    "Your API tokens",
  ] as const;
  const [tab, setTab] = useState<string>(tabs[0]);
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
      {tab === "People" && <People />}
      {tab === "Join tokens" && <JoinTokens />}
      {tab === "Certificates" && <Certificates />}
      {tab === "Your API tokens" && <ApiTokens />}
    </>
  );
}
