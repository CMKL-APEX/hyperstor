import { useState, type FormEvent } from "react";
import { useAuthApi } from "./auth";
import { isHttpError } from "./client";

const inputClass =
  "mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent";

/** Username and password for a PAM login (also the re-login dialog). */
export function PamForm({
  initialUsername = "",
  submitLabel,
  onDone,
}: {
  initialUsername?: string;
  submitLabel: string;
  onDone: () => void;
}) {
  const auth = useAuthApi();
  const [username, setUsername] = useState(initialUsername);
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setSubmitting(true);
    setError(null);
    try {
      await auth.login(username, password);
      setPassword("");
      onDone();
    } catch (err) {
      setError(isHttpError(err) && err.code === "login_failed" ? "Wrong username or password." : String(err instanceof Error ? err.message : err));
      setSubmitting(false);
    }
  };

  return (
    <form onSubmit={submit} className="space-y-4">
      <div>
        <label htmlFor="login-username" className="block text-sm font-medium text-gray-700">
          Username
        </label>
        <input
          id="login-username"
          className={inputClass}
          autoComplete="username"
          required
          value={username}
          onChange={(e) => setUsername(e.target.value)}
        />
      </div>
      <div>
        <label htmlFor="login-password" className="block text-sm font-medium text-gray-700">
          Password
        </label>
        <input
          id="login-password"
          type="password"
          className={inputClass}
          autoComplete="current-password"
          required
          value={password}
          onChange={(e) => setPassword(e.target.value)}
        />
      </div>
      {error && <p className="text-sm text-red-600">{error}</p>}
      <button
        type="submit"
        disabled={submitting}
        className="w-full px-4 py-2 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg disabled:opacity-50"
      >
        {submitting ? "Signing in..." : submitLabel}
      </button>
    </form>
  );
}
