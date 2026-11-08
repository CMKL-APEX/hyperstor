// The build the mgmt serving this UI came from (19-administration.md
// §23.7): its release (the version tag, or the release it follows and how
// far), branch and commit, linked to the code on GitHub.
import type { Build } from "./api";

type Part = { label: string; value: string; href?: string; title?: string };

export default function Footer({ build }: { build: Build | null }) {
  const parts: Part[] = [];
  if (build) {
    const { tag, release, ahead, branch, commit, repo } = build;
    const enc = encodeURIComponent;
    if (tag) {
      parts.push({ label: "Version", value: tag, href: `${repo}/releases/tag/${enc(tag)}`, title: `The ${tag} release` });
    } else if (release) {
      parts.push({
        label: "Version",
        value: `${release}+${ahead}`,
        // What this build has on top of the release.
        href: commit ? `${repo}/compare/${enc(release)}...${commit}` : `${repo}/releases/tag/${enc(release)}`,
        title: `${ahead} commit${ahead === 1 ? "" : "s"} after ${release}`,
      });
    }
    if (branch) parts.push({ label: "Branch", value: branch });
    if (commit)
      parts.push({ label: "Commit", value: commit.slice(0, 10), href: `${repo}/commit/${commit}`, title: "This commit on GitHub" });
  }
  return (
    <footer className="border-t border-slate-200 dark:border-slate-700 mt-8" data-testid="build-info">
      <div className="max-w-6xl mx-auto px-5 py-4 flex flex-wrap items-center gap-x-4 gap-y-1 text-xs text-slate-500">
        <span className="font-medium text-slate-600 dark:text-slate-300">Hyperstor</span>
        {parts.length === 0 && <span>build information unavailable</span>}
        {parts.map((p) => (
          <span key={p.label}>
            {p.label}{" "}
            {p.href ? (
              <a
                href={p.href}
                target="_blank"
                rel="noopener noreferrer"
                title={p.title}
                className="font-mono text-slate-700 dark:text-slate-200 underline decoration-slate-300 hover:text-sky-700 hover:decoration-sky-700"
              >
                {p.value}
              </a>
            ) : (
              <span className="font-mono text-slate-700 dark:text-slate-200">{p.value}</span>
            )}
          </span>
        ))}
      </div>
    </footer>
  );
}
