# @glidex/ui-kit

The web UI pieces glidex's UI and hyperstor's management UI share
(hyperstor decision D97). TypeScript and React source, consumed by the
app's bundler directly: no build step, not published (`private`).

| Export | What |
|---|---|
| `Modal` | The dialog frame: title, close button, backdrop. |
| `TokenReveal` | A secret shown once: masked until asked, Copy to the clipboard. The app passes the warning, the hint after copying, and what follows (`children`). |
| `PamForm` | Username and password form; calls `AuthApi.login`. |
| `SsoButtons` | "Sign in with <label>", one per `SsoProvider {id?, label}`; calls `AuthApi.startSso`. |
| `ReauthDialog` | Step-up: "Confirm it's you" with `PamForm`, `SsoButtons` and the app's extra ways (`children`, say a passkey button). |
| `SessionProvider`, `useSession` | whoami; the login page when nobody is signed in; the error panel with Retry; the step-up dialog when a request gets `401 reauth_required`. |
| `csrfToken`, `setCsrf` | The session's CSRF value (memory only), for the app's request headers. |
| `withSession`, `reauthenticate`, `sessionEnded` | The 401 flow for the app's API client: `reauth_required` asks the user to log in again and sends once more; `unauthenticated` returns to the login page. |
| `isHttpError`, `AuthApiProvider`, `useAuthApi` | Helpers. |

## The adapter

The app gives `SessionProvider` its login API; nothing here knows the
app's routes or types.

```ts
interface AuthApi<W extends WhoAmIBase, M> {
  methods(): Promise<M>;                 // what the login page offers
  whoami(): Promise<W>;                  // throws a 401 when signed out
  login(username: string, password: string): Promise<unknown>;
  startSso(provider: string | undefined, returnTo: string, reauth: boolean): void;
  logout(): Promise<void>;
}
interface WhoAmIBase { csrf?: string | null; user?: { display_name?: string } | null }
```

`W` is the app's whoami and `M` its login methods; errors carry `status`
and `code` (`login_failed` shows "Wrong username or password.").

```tsx
<SessionProvider
  auth={auth}                       // a stable object
  load={load}                       // optional, stable: (me) => Promise<X>, the app's extras
  login={(methods, onLoggedIn) => <Login ... />}
  reauth={({ methods, me, onDone, onCancel }) => <ReauthDialog ... />}
  loading={<Loading />}
  unreachable="Can't reach the control plane"
>
  {/* rendered once signed in; useSession<W, M, X>() gives me, methods, extra, setExtra, refresh, logout */}
</SessionProvider>
```

The app's API client sends `csrfToken()` on writes and wraps its calls
in `withSession` (or calls `reauthenticate` / `sessionEnded` itself).

## Using it

Glidex's UI (`../ui`) lists this directory as a bun workspace
(`"workspaces": ["../kit"]`, `"@glidex/ui-kit": "workspace:*"`) and adds
`../kit/src` to Tailwind's `content`, so the classes used here are
generated. `react` and `@types/react` are peer dependencies: the app's.
The kit's classes are Tailwind 3 defaults, as glidex's.
