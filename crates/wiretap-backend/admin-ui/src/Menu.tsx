import { type KeyboardEvent, useId, useLayoutEffect, useRef, useState } from "react";

export type MenuItem =
  | { label: string; onSelect: () => void; danger?: boolean; unavailable?: string }
  | "separator";

const STEPS: Record<string, (at: number, last: number) => number> = {
  ArrowDown: (at, last) => (at >= last ? 0 : at + 1),
  ArrowUp: (at, last) => (at <= 0 ? last : at - 1),
  Home: () => 0,
  End: (_, last) => last,
};

/**
 * A kebab button and its menu, on `popover` so light dismiss, Escape and the
 * top layer (no clipping by a scrolling table) are the platform's. The trigger
 * is the panel's `popovertarget`, so the platform knows not to light-dismiss on
 * the trigger's own press. Items are built only while it is open.
 */
export default function Menu({ label, items }: { label: string; items: MenuItem[] }) {
  const id = useId();
  const trigger = useRef<HTMLButtonElement>(null);
  const panel = useRef<HTMLDivElement>(null);
  const [open, setOpen] = useState(false);
  const entries = () => [...(panel.current?.querySelectorAll<HTMLElement>('[role="menuitem"]') ?? [])];
  const dismiss = () => panel.current?.hidePopover();

  // Once the items are in, before paint; measured at the origin, where no
  // label wraps against the viewport's edge. A fixed panel does not follow the
  // row it was placed against, so a scroll or resize closes it.
  useLayoutEffect(() => {
    if (!open) return;
    const p = panel.current!;
    p.style.top = p.style.left = "0px";
    const at = trigger.current!.getBoundingClientRect();
    const roomBelow = window.innerHeight - at.bottom;
    p.style.top = `${roomBelow < p.offsetHeight + 8 ? at.top - p.offsetHeight - 4 : at.bottom + 4}px`;
    p.style.left = `${Math.max(8, at.right - p.offsetWidth)}px`;
    p.style.visibility = "";
    entries()[0]?.focus({ preventScroll: true });
    window.addEventListener("scroll", dismiss, { capture: true, passive: true });
    window.addEventListener("resize", dismiss);
    return () => {
      window.removeEventListener("scroll", dismiss, { capture: true });
      window.removeEventListener("resize", dismiss);
    };
  }, [open]);

  const navigate = (e: KeyboardEvent) => {
    if (e.key === "Tab") {
      dismiss();
      return;
    }
    const step = STEPS[e.key];
    if (!step) return;
    e.preventDefault();
    const all = entries();
    all[step(all.indexOf(document.activeElement as HTMLElement), all.length - 1)]?.focus();
  };

  return (
    <>
      <button
        ref={trigger}
        className="icon"
        popoverTarget={id}
        aria-label={label}
        aria-haspopup="menu"
        aria-expanded={open}
      >
        ⋮
      </button>
      <div
        ref={panel}
        id={id}
        popover="auto"
        role="menu"
        aria-label={label}
        className="menu-list"
        onKeyDown={navigate}
        onBeforeToggle={(e) => {
          if (e.newState === "open") panel.current!.style.visibility = "hidden";
        }}
        onToggle={(e) => setOpen(e.newState === "open")}
      >
        {open &&
          items.map((item, i) =>
            item === "separator" ? (
              <div key={i} role="separator" />
            ) : (
              <button
                key={item.label}
                role="menuitem"
                className={item.danger ? "danger" : undefined}
                aria-disabled={item.unavailable ? true : undefined}
                onClick={() => {
                  if (item.unavailable) return;
                  // Closed and focus handed back first, so a dialog the item opens
                  // returns focus to the trigger rather than to a hidden item.
                  dismiss();
                  trigger.current?.focus();
                  item.onSelect();
                }}
              >
                {item.label}
                {item.unavailable && <small> — {item.unavailable}</small>}
              </button>
            ),
          )}
      </div>
    </>
  );
}
