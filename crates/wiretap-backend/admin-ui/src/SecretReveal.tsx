import { useRef, useState } from "react";

/** A secret shown once, in full, with a copy glyph. */
export default function SecretReveal({ value, note }: { value: string; note: string }) {
  const [copied, setCopied] = useState<string | null>(null);
  const shown = useRef<HTMLSpanElement>(null);

  // navigator.clipboard needs a secure context, and the admin UI is often plain http://<host>:8423.
  const copy = async () => {
    if (window.isSecureContext && navigator.clipboard) {
      await navigator.clipboard.writeText(value);
    } else {
      window.getSelection()?.selectAllChildren(shown.current!);
      if (!document.execCommand("copy")) return;
    }
    setCopied(value);
  };

  return (
    <div className="keyreveal">
      <div className="muted" style={{ marginBottom: "0.4rem" }}>
        {note}
      </div>
      <span ref={shown} className="mono">
        {value}
      </span>{" "}
      <button
        className="icon"
        aria-label="Copy to clipboard"
        title={copied === value ? "Copied" : "Copy to clipboard"}
        onClick={copy}
      >
        {copied === value ? "✓" : "⧉"}
      </button>
    </div>
  );
}
