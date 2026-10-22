import { useAuthApi } from "./auth";

/** One single sign-on choice: `id` goes to AuthApi.startSso (undefined:
 * the server's default), `label` after "Sign in with". */
export interface SsoProvider {
  id?: string;
  label: string;
}

/** Where to come back to after a login elsewhere: this page. */
export function currentPath(): string {
  return window.location.pathname + window.location.search;
}

/** "Sign in with <label>", one button per provider; `reauth` for a step-up. */
export function SsoButtons({ providers, reauth = false }: { providers: SsoProvider[]; reauth?: boolean }) {
  const auth = useAuthApi();
  return (
    <div className="space-y-2">
      {providers.map((b) => (
        <button
          key={b.id ?? "sso"}
          type="button"
          onClick={() => auth.startSso(b.id, currentPath(), reauth)}
          className="w-full px-4 py-2 text-sm font-medium text-sky-700 bg-white border border-sky-300 hover:bg-sky-50 rounded-lg"
        >
          Sign in with {b.label}
        </button>
      ))}
    </div>
  );
}
