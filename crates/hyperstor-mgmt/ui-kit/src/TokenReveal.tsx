import { useState, type ReactNode } from "react";

/** A secret shown once: masked until asked, copied to the clipboard on
 * request, never put in a URL, storage or a command line. `warning` says
 * what it is and how long it works; `copiedHint` what to do once copied;
 * `children` follow (how to use it). The caller drops the secret when the
 * dialog closes. */
export default function TokenReveal({
  secret,
  warning,
  copiedHint,
  label = "token",
  children,
}: {
  secret: string;
  warning: ReactNode;
  copiedHint: ReactNode;
  /** The field's accessible name. */
  label?: string;
  children?: ReactNode;
}) {
  const [shown, setShown] = useState(false);
  const [copied, setCopied] = useState<"ok" | "failed" | null>(null);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(secret);
      setCopied("ok");
    } catch {
      setCopied("failed");
    }
  };
  return (
    <div className="space-y-3 text-sm">
      <p className="text-gray-700">{warning}</p>
      <div className="flex gap-2">
        <input
          readOnly
          aria-label={label}
          type={shown ? "text" : "password"}
          autoComplete="off"
          spellCheck={false}
          className="flex-1 px-3 py-2 border border-gray-300 rounded-lg font-mono text-xs bg-gray-50"
          value={secret}
          onFocus={(e) => e.currentTarget.select()}
        />
        <button type="button" className="px-3 py-2 text-xs bg-gray-100 hover:bg-gray-200 rounded-lg" onClick={() => setShown((s) => !s)}>
          {shown ? "Hide" : "Show"}
        </button>
        <button type="button" className="px-3 py-2 text-xs text-white bg-sky-600 hover:bg-sky-700 rounded-lg" onClick={copy}>
          Copy
        </button>
      </div>
      {copied === "ok" && <p className="text-xs text-green-700">{copiedHint}</p>}
      {copied === "failed" && <p className="text-xs text-amber-700">The browser refused the clipboard; select the field and copy it.</p>}
      {children}
    </div>
  );
}
