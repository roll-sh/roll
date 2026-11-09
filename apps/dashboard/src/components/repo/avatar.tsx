/** A neutral monogram for an owner: initials on a flat panel, the same for a name everywhere. */
export function Monogram({ name, size = 40, org = false }: { name: string; size?: number; org?: boolean }) {
  const parts = name.split(/[-_.\s]+/).filter(Boolean);
  const initials = (parts.length > 1 ? parts[0][0] + parts[1][0] : name[0] ?? "?").toUpperCase();
  return (
    <span
      aria-hidden
      className={`grid shrink-0 place-items-center border border-line bg-panel-2 font-semibold tracking-[-0.04em] text-fg ${
        org ? "rounded-[22%]" : "rounded-full"
      }`}
      style={{ width: size, height: size, fontSize: Math.round(size * 0.36) }}
    >
      {initials}
    </span>
  );
}
