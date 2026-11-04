"use client";

/**
 * Ported from T3 Code's `components/ui/select.tsx` (MIT), in roll's colours: a button that opens
 * a list, in place of a native `<select>`. `Select` is the one-line form most fields want.
 */

import { Select as SelectPrimitive } from "@base-ui/react/select";
import { Check, ChevronDown, ChevronUp } from "lucide-react";
import type { ReactNode } from "react";
import { cn, mergeClass, glass, itemRow, popupMotion } from "./cn";

export const SelectRoot = SelectPrimitive.Root;
export const SelectGroup = SelectPrimitive.Group;

const triggers = {
  /** A form field: a hairline box. */
  default: "min-h-8 w-full min-w-36 rounded-lg border border-line-2 bg-bg px-2.5 text-[13px] text-fg hover:border-subtle focus-visible:border-subtle",
  /** A chip in a toolbar (the composer's): no box until hovered. */
  ghost: "h-7 gap-1 rounded-md border border-transparent px-1.5 text-[12px] text-muted hover:bg-fg/[0.06] hover:text-fg data-popup-open:bg-fg/[0.06] data-popup-open:text-fg",
} as const;

export type SelectVariant = keyof typeof triggers;

export function SelectTrigger({ className, variant = "default", children, ...props }: SelectPrimitive.Trigger.Props & { variant?: SelectVariant }) {
  return (
    <SelectPrimitive.Trigger
      className={mergeClass(
        cn(
          "relative inline-flex cursor-pointer items-center justify-between gap-2 text-left outline-none transition-colors select-none data-disabled:pointer-events-none data-disabled:opacity-60 [&_svg]:shrink-0",
          triggers[variant],
        ),
        className,
      )}
      {...props}
    >
      {children}
      <SelectPrimitive.Icon className="flex">
        <ChevronDown className="-me-0.5 size-3.5 opacity-70" />
      </SelectPrimitive.Icon>
    </SelectPrimitive.Trigger>
  );
}

export function SelectValue({ className, ...props }: SelectPrimitive.Value.Props) {
  return <SelectPrimitive.Value className={mergeClass(cn("min-w-0 flex-1 truncate data-placeholder:text-subtle"), className)} {...props} />;
}

export function SelectPopup({
  className,
  children,
  side = "bottom",
  sideOffset = 4,
  align = "start",
  alignItemWithTrigger = false,
}: {
  className?: string;
  children: ReactNode;
  side?: SelectPrimitive.Positioner.Props["side"];
  sideOffset?: number;
  align?: SelectPrimitive.Positioner.Props["align"];
  alignItemWithTrigger?: boolean;
}) {
  return (
    <SelectPrimitive.Portal>
      <SelectPrimitive.Positioner className="z-[130] select-none" side={side} sideOffset={sideOffset} align={align} alignItemWithTrigger={alignItemWithTrigger}>
        <SelectPrimitive.Popup className={cn("rounded-lg text-fg outline-none", popupMotion)}>
          <SelectPrimitive.ScrollUpArrow className="top-0 z-10 flex h-6 w-full cursor-default items-center justify-center">
            <ChevronUp className="size-3.5" />
          </SelectPrimitive.ScrollUpArrow>
          <div className={cn("relative h-full min-w-(--anchor-width) rounded-lg", glass)}>
            <SelectPrimitive.List className={mergeClass(cn("max-h-(--available-height) overflow-y-auto p-1"), className)}>{children}</SelectPrimitive.List>
          </div>
          <SelectPrimitive.ScrollDownArrow className="bottom-0 z-10 flex h-6 w-full cursor-default items-center justify-center">
            <ChevronDown className="size-3.5" />
          </SelectPrimitive.ScrollDownArrow>
        </SelectPrimitive.Popup>
      </SelectPrimitive.Positioner>
    </SelectPrimitive.Portal>
  );
}

export function SelectItem({ className, children, hint, ...props }: SelectPrimitive.Item.Props & { hint?: ReactNode }) {
  return (
    <SelectPrimitive.Item className={mergeClass(cn(itemRow, "data-selected:bg-fg/[0.06]"), className)} {...props}>
      <SelectPrimitive.ItemText className="min-w-0 flex-1 truncate">{children}</SelectPrimitive.ItemText>
      {hint && <span className="shrink-0 text-[11.5px] text-subtle">{hint}</span>}
      <SelectPrimitive.ItemIndicator className="flex shrink-0">
        <Check className="size-3.5" />
      </SelectPrimitive.ItemIndicator>
    </SelectPrimitive.Item>
  );
}

export function SelectGroupLabel({ className, ...props }: SelectPrimitive.GroupLabel.Props) {
  return <SelectPrimitive.GroupLabel className={mergeClass(cn("px-2 py-1.5 text-[11.5px] font-medium text-subtle"), className)} {...props} />;
}

export interface SelectOption<V extends string = string> {
  value: V;
  label: string;
  hint?: ReactNode;
  disabled?: boolean;
}

/**
 * A select in one line: `<Select label="Machine" value={id} onChange={setId} options={[…]} />`.
 * The trigger is a button named `label` showing the picked option's label.
 */
export function Select<V extends string>({
  label,
  value,
  onChange,
  options,
  variant = "default",
  placeholder,
  disabled,
  className,
  icon,
  popupClassName,
  title,
}: {
  label: string;
  value: V | null;
  onChange: (value: V) => void;
  options: readonly SelectOption<V>[];
  variant?: SelectVariant;
  placeholder?: string;
  disabled?: boolean;
  className?: string;
  /** Before the value, in the trigger. */
  icon?: ReactNode;
  popupClassName?: string;
  /** The trigger's tooltip (default: `label`). */
  title?: string;
}) {
  const items = options.map((o) => ({ value: o.value, label: o.label }));
  return (
    <SelectPrimitive.Root<V> value={value} onValueChange={(v) => v !== null && onChange(v)} items={items} disabled={disabled}>
      <SelectTrigger variant={variant} className={className} aria-label={label} title={title ?? label}>
        {icon}
        <SelectValue placeholder={placeholder ?? label} />
      </SelectTrigger>
      <SelectPopup className={popupClassName}>
        {options.map((o) => (
          <SelectItem key={o.value} value={o.value} disabled={o.disabled} hint={o.hint}>
            {o.label}
          </SelectItem>
        ))}
      </SelectPopup>
    </SelectPrimitive.Root>
  );
}
