import { useId, type ReactNode } from "react";

interface ModalProps {
  title: string;
  onClose: () => void;
  children: ReactNode;
}

export default function Modal({ title, onClose, children }: ModalProps) {
  const titleId = useId();
  return (
    <div className="fixed inset-0 z-50 overflow-y-auto">
      <div className="flex min-h-screen items-center justify-center p-4">
        <div
          className="fixed inset-0 bg-black bg-opacity-50 transition-opacity"
          onClick={onClose}
        />
        <div role="dialog" aria-modal="true" aria-labelledby={titleId} className="relative bg-white rounded-xl shadow-xl max-w-lg w-full p-6 z-10">
          <div className="flex items-center justify-between mb-4">
            <h2 id={titleId} className="text-xl font-semibold text-gray-900">{title}</h2>
            <button
              className="text-gray-400 hover:text-gray-600 transition-colors"
              aria-label="Close"
              onClick={onClose}
            >
              <svg
                className="w-6 h-6"
                fill="none"
                stroke="currentColor"
                viewBox="0 0 24 24"
              >
                <path
                  strokeLinecap="round"
                  strokeLinejoin="round"
                  strokeWidth="2"
                  d="M6 18L18 6M6 6l12 12"
                />
              </svg>
            </button>
          </div>
          {children}
        </div>
      </div>
    </div>
  );
}
