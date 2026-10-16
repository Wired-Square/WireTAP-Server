import { type ReactNode, useId, useLayoutEffect, useRef, useState } from "react";

export interface Question {
  title: string;
  message: ReactNode;
  /** Names the act, e.g. "Revoke", never "OK". */
  verb: string;
  danger?: boolean;
  /** Resolve and the dialog closes; throw and it stays open with the refusal. */
  act: () => Promise<void> | void;
}

/** A modal "are you sure" on `<dialog>`: Escape cancels and focus returns to whatever opened it. */
function Confirm({ title, message, verb, danger, act, onClose }: Question & { onClose: () => void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const titleId = useId();
  const [pending, setPending] = useState(false);
  const [error, setError] = useState("");

  // Layout, not passive: the cleanup must run while the dialog is still in the
  // document, or close() can't hand focus back.
  useLayoutEffect(() => {
    const opener = document.activeElement as HTMLElement | null;
    dialog.current?.showModal();
    return () => {
      dialog.current?.close();
      opener?.focus();
    };
  }, []);

  const confirm = async () => {
    if (pending) return;
    setPending(true);
    setError("");
    try {
      await act();
      onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setPending(false);
    }
  };

  // A browser honours preventDefault on `cancel` only once per user activation,
  // so a second Escape closes the element outright.
  const closedByPlatform = () => {
    const d = dialog.current;
    if (!d || d.open) return;
    if (pending) d.showModal();
    else onClose();
  };

  return (
    <dialog
      ref={dialog}
      className="confirm"
      aria-labelledby={titleId}
      onCancel={(e) => {
        e.preventDefault();
        if (!pending) onClose();
      }}
      onClose={closedByPlatform}
    >
      <h2 id={titleId}>{title}</h2>
      {error && (
        <p className="error" role="alert">
          {error}
        </p>
      )}
      <div>{message}</div>
      {/* Cancel first, so the focus showModal() gives is not the act. */}
      <div className="confirm-actions">
        <button className="btn" aria-disabled={pending} onClick={() => pending || onClose()}>
          Cancel
        </button>
        <button className={`btn ${danger ? "danger" : "primary"}`} aria-disabled={pending} onClick={confirm}>
          {verb}
        </button>
      </div>
    </dialog>
  );
}

/** The dialog to render, and `ask` to open it with a question. */
export function useConfirm(): [ReactNode, (question: Question) => void] {
  const [question, ask] = useState<Question | null>(null);
  return [question && <Confirm {...question} onClose={() => ask(null)} />, ask];
}
