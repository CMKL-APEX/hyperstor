import type { ReactNode } from "react";
import Modal from "./Modal";
import { PamForm } from "./PamForm";
import { SsoButtons, type SsoProvider } from "./SsoButtons";

/** Step-up (401 reauth_required): log in again, then the request is
 * retried. `pam` offers the password form, `sso` the single sign-on
 * buttons (none: null or empty); `children` go below them (other ways to
 * sign in, such as a passkey button). */
export default function ReauthDialog({
  username,
  pam,
  sso,
  onDone,
  onCancel,
  children,
}: {
  username: string;
  pam: boolean;
  sso?: SsoProvider[] | null;
  onDone: () => void;
  onCancel: () => void;
  children?: ReactNode;
}) {
  const offersSso = !!sso && sso.length > 0;
  return (
    <Modal title="Confirm it's you" onClose={onCancel}>
      <p className="mb-4 text-sm text-gray-600">
        This action needs a recent login. Sign in again to continue.
      </p>
      {pam && <PamForm initialUsername={username} submitLabel="Sign in and retry" onDone={onDone} />}
      {pam && offersSso && <div className="my-3" />}
      {offersSso && <SsoButtons providers={sso} reauth />}
      {children}
      <button className="mt-3 w-full px-4 py-2 text-sm text-gray-600 hover:bg-gray-100 rounded-lg" onClick={onCancel}>
        Cancel
      </button>
    </Modal>
  );
}
