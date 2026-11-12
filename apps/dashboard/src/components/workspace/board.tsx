import { HardDrive, LoaderCircle } from "lucide-react";
import { useRef, type PointerEvent as ReactPointerEvent } from "react";
import type { ResourceKind } from "@/lib/workspace-api";
import { CARD_H, CARD_W, KIND_BLURB, KIND_LABEL, type ResourceView } from "@/lib/workspace-resources";
import type { Staged } from "@/lib/workspace-store";
import { KindTile } from "./kind";

/** A card on the board: a placed resource (with its view) or a staged one. */
export type Card =
  | { type: "placed"; key: string; x: number; y: number; view: ResourceView }
  | { type: "staged"; key: string; x: number; y: number; staged: Staged };

const DOT: Record<string, string> = {
  online: "bg-ok",
  idle: "bg-subtle",
  starting: "bg-accent animate-pulse motion-reduce:animate-none",
  missing: "bg-danger",
};

/**
 * The project's canvas: cards where the workspace put them, on the dot grid. Drag a card to move
 * it (the position is saved on release), click one to open it.
 */
export function Board({
  cards,
  shift = 0,
  selected,
  onSelect,
  onMove,
  children,
}: {
  cards: Card[];
  /** Move the cards right by this much (a panel open on the left), sliding with it. */
  shift?: number;
  selected: string | null;
  onSelect: (key: string | null) => void;
  onMove: (card: Card, x: number, y: number) => void;
  children?: React.ReactNode;
}) {
  const width = Math.max(0, ...cards.map((c) => c.x + CARD_W)) + 160;
  const height = Math.max(0, ...cards.map((c) => c.y + CARD_H)) + 160;
  return (
    <div
      className="absolute inset-0 overflow-auto bg-[radial-gradient(var(--line-3)_1px,transparent_1.3px)] [background-size:20px_20px] [scrollbar-width:thin]"
      onPointerDown={(e) => e.target === e.currentTarget && onSelect(null)}
    >
      <div
        className="relative min-h-full min-w-full transition-transform duration-300 ease-drawer motion-reduce:transition-none"
        style={{ width, height, transform: shift ? `translateX(${shift}px)` : undefined }}
        onPointerDown={(e) => e.target === e.currentTarget && onSelect(null)}>
        {cards.map((c) => (
          <Node key={c.key} card={c} selected={selected === c.key} onSelect={() => onSelect(c.key)} onMove={(x, y) => onMove(c, x, y)} />
        ))}
        {children}
      </div>
    </div>
  );
}

function Node({ card, selected, onSelect, onMove }: { card: Card; selected: boolean; onSelect: () => void; onMove: (x: number, y: number) => void }) {
  const el = useRef<HTMLDivElement>(null);

  const down = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (e.button !== 0 || !el.current) return;
    const node = el.current;
    const start = { x: e.clientX, y: e.clientY };
    let at = { x: card.x, y: card.y };
    let moved = false;
    node.setPointerCapture(e.pointerId);
    const move = (ev: PointerEvent) => {
      const dx = ev.clientX - start.x;
      const dy = ev.clientY - start.y;
      if (!moved && Math.hypot(dx, dy) < 4) return;
      moved = true;
      node.dataset.dragging = "";
      at = { x: Math.max(0, Math.round(card.x + dx)), y: Math.max(0, Math.round(card.y + dy)) };
      node.style.transform = `translate(${at.x - card.x}px, ${at.y - card.y}px)`;
    };
    const up = () => {
      node.removeEventListener("pointermove", move);
      node.removeEventListener("pointerup", up);
      node.removeEventListener("pointercancel", up);
      delete node.dataset.dragging;
      node.style.transform = "";
      if (moved) onMove(at.x, at.y);
      else onSelect();
    };
    node.addEventListener("pointermove", move);
    node.addEventListener("pointerup", up);
    node.addEventListener("pointercancel", up);
  };

  const staged = card.type === "staged" ? card.staged : null;
  const view = card.type === "placed" ? card.view : null;
  const kind: ResourceKind = staged ? staged.kind : view!.kind;

  return (
    <div
      ref={el}
      onPointerDown={down}
      onKeyDown={(e) => (e.key === "Enter" || e.key === " ") && (e.preventDefault(), onSelect())}
      tabIndex={0}
      role="button"
      aria-pressed={selected}
      className="group rise absolute cursor-grab touch-none outline-none select-none data-[dragging]:z-10 data-[dragging]:cursor-grabbing"
      style={{ left: card.x, top: card.y, width: CARD_W }}
    >
      <div
        className={`rounded-[10px] border bg-sheet shadow-card inset-shadow-highlight transition-[border-color,box-shadow] duration-150 group-hover:shadow-card-hover group-focus-visible:border-accent/70 group-data-[dragging]:shadow-card-hover ${
          staged
            ? `border-dashed ${staged.phase === "failed" ? "border-danger/60" : "border-accent/60"} bg-sheet/80`
            : selected
              ? "border-accent/70"
              : "border-line-2 group-hover:border-line-3"
        }`}
      >
        <div className="flex items-start gap-2 px-2.5 pt-2.5 pb-2">
          <KindTile kind={kind} />
          <div className="min-w-0 flex-1 leading-tight">
            <div className="truncate text-[12.5px] font-semibold tracking-[-0.01em]">{staged ? staged.name : view!.name}</div>
            <div className="mt-0.5 truncate font-mono text-[10.5px] text-muted">{staged ? KIND_BLURB[kind] : view!.sub}</div>
          </div>
        </div>
        <div className="flex items-center gap-1.5 border-t border-line px-2.5 py-1.5 text-[11px] whitespace-nowrap text-muted">
          {staged ? (
            staged.phase === "creating" ? (
              <>
                <LoaderCircle className="size-3 animate-spin text-accent motion-reduce:animate-none" />
                Creating
              </>
            ) : staged.phase === "failed" ? (
              <>
                <i className="size-1.5 rounded-full bg-danger" />
                <span className="truncate text-danger">Failed · {staged.error}</span>
              </>
            ) : (
              <>
                <i className="size-1.5 rounded-full shadow-[inset_0_0_0_1.25px_var(--accent)]" />
                Staged · {KIND_LABEL[kind]}
              </>
            )
          ) : (
            <>
              <i className={`size-1.5 shrink-0 rounded-full ${DOT[view!.state]}`} />
              <span className="truncate">{view!.stateLabel}</span>
              <span className="ml-auto font-mono text-[10.5px] text-subtle">{view!.meta}</span>
            </>
          )}
        </div>
      </div>
      {view?.attached && (
        <div className="mx-3 flex items-center gap-1.5 rounded-b-lg border border-t-0 border-line-2 bg-canvas px-2 py-1 font-mono text-[10px] text-muted">
          <HardDrive className="size-3" />
          <span className="truncate">{view.attached}</span>
        </div>
      )}
    </div>
  );
}
