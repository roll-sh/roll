"use client";

/** Ported from T3 Code's `components/ui/toggle-group.tsx` and `toggle.tsx` (MIT): the segmented control. */

import { Toggle as TogglePrimitive } from "@base-ui/react/toggle";
import { ToggleGroup as ToggleGroupPrimitive } from "@base-ui/react/toggle-group";
import type { ReactNode } from "react";
import { cn, mergeClass } from "./cn";

export function ToggleGroup({ className, ...props }: ToggleGroupPrimitive.Props) {
  return <ToggleGroupPrimitive className={mergeClass(cn("flex w-fit gap-0.5 rounded-lg bg-fg/[0.06] p-0.5"), className)} {...props} />;
}

export function Toggle({ className, ...props }: TogglePrimitive.Props) {
  return (
    <TogglePrimitive
      className={mergeClass(
        cn(
          "inline-flex h-6 min-w-0 cursor-pointer items-center justify-center gap-1.5 rounded-md px-2.5 text-[12px] whitespace-nowrap text-muted outline-none transition-colors select-none hover:text-fg focus-visible:ring-2 focus-visible:ring-accent/60 disabled:pointer-events-none disabled:opacity-60 data-pressed:bg-bg data-pressed:text-fg data-pressed:shadow-sm dark:data-pressed:bg-line-2 [&_svg]:size-3.5 [&_svg]:shrink-0",
        ),
        className,
      )}
      {...props}
    />
  );
}

/** One of a few, as T3's segmented controls: `<Segmented label="While running" value={m} onChange={setM} options={[…]} />`. */
export function Segmented<V extends string>({
  label,
  value,
  onChange,
  options,
  className,
}: {
  label: string;
  value: V;
  onChange: (value: V) => void;
  options: readonly {
    value: V;
    label: ReactNode;
    title?: string;
    disabled?: boolean;
  }[];
  className?: string;
}) {
  return (
    <ToggleGroup aria-label={label} value={[value]} onValueChange={(v) => v[0] !== undefined && onChange(v[0] as V)} className={className}>
      {options.map((o) => (
        <Toggle key={o.value} value={o.value} title={o.title} disabled={o.disabled}>
          {o.label}
        </Toggle>
      ))}
    </ToggleGroup>
  );
}
