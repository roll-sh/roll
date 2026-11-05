"use client";

/**
 * A phone's keyboard has no Esc, Ctrl, Tab or arrows: the row of them a terminal shows under
 * itself there (like Termius's and Blink's). Ctrl is sticky: it applies to the next key typed (or
 * tapped here), then lets go. `send` writes bytes to the PTY; the terminal applies a held Ctrl to
 * what's typed through [`withCtrl`].
 */
export function TerminalKeys({ send, ctrl, setCtrl }: { send: (data: string) => void; ctrl: boolean; setCtrl: (on: boolean) => void }) {
  const key = (label: string, data: string, aria: string) => (
    <button
      key={aria}
      type="button"
      aria-label={aria}
      // Keep the keyboard up: a key never takes focus from the terminal.
      onPointerDown={(e) => e.preventDefault()}
      onClick={() => {
        send(ctrl ? withCtrl(data) : data);
        setCtrl(false);
      }}
      className="h-8 min-w-9 shrink-0 rounded-md border border-line-2 bg-panel-2 px-2 font-mono text-[12px] text-soft active:bg-fg/[0.08]"
    >
      {label}
    </button>
  );
  return (
    <div role="toolbar" aria-label="Terminal keys" className="flex gap-1 overflow-x-auto border-t border-line px-2 py-1.5 [scrollbar-width:none]" data-nodrag>
      {key("esc", "\x1b", "Escape")}
      {key("tab", "\t", "Tab")}
      <button
        type="button"
        aria-label="Control"
        aria-pressed={ctrl}
        onPointerDown={(e) => e.preventDefault()}
        onClick={() => setCtrl(!ctrl)}
        className={`h-8 min-w-9 shrink-0 rounded-md border px-2 font-mono text-[12px] active:bg-fg/[0.08] ${ctrl ? "border-accent/60 bg-accent/15 text-accent" : "border-line-2 bg-panel-2 text-soft"}`}
      >
        ctrl
      </button>
      {key("^C", "\x03", "Control C")}
      {key("←", "\x1b[D", "Left")}
      {key("↑", "\x1b[A", "Up")}
      {key("↓", "\x1b[B", "Down")}
      {key("→", "\x1b[C", "Right")}
      {key("/", "/", "Slash")}
      {key("-", "-", "Dash")}
      {key("|", "|", "Pipe")}
      {key("~", "~", "Tilde")}
    </div>
  );
}

/** What Ctrl does to one typed character (`c` is ^C, `[` is Esc, space is NUL); anything else goes as it is. */
export function withCtrl(data: string): string {
  if (data === " ") return "\x00";
  if (data.length !== 1) return data;
  const code = data.toUpperCase().charCodeAt(0);
  return code >= 0x40 && code <= 0x5f ? String.fromCharCode(code & 0x1f) : data;
}
