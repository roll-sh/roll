/**
 * T3 Code's controls for the agents surface (ported from T3's `components/ui`, MIT, on Base UI):
 * use these instead of native `<select>`, checkboxes and hand-made popovers.
 *
 * - `Select` (one line) or `SelectRoot`/`SelectTrigger`/`SelectPopup`/`SelectItem` (parts)
 * - `Menu`, `MenuTrigger`, `MenuPopup`, `MenuItem`, `MenuRadioGroup`/`MenuRadioItem`, `MenuCheckboxItem`, submenus
 * - `Popover`, `PopoverTrigger`, `PopoverPopup`
 * - `Combobox`, `ComboboxSearchInput`, `ComboboxList`, `ComboboxItem`, `ComboboxEmpty`
 * - `Switch`; `Segmented` (or `ToggleGroup`/`Toggle`)
 *
 * Popups portal to the body above the agents popup, carrying the agents tokens when opened from the
 * surface (`ui/layer.tsx`), grow out of their trigger and honour reduced motion. Pass a trigger
 * element with `render={<button … />}` as Base UI does.
 */
export * from "./cn";
export * from "./combobox";
export * from "./menu";
export * from "./popover";
export * from "./select";
export * from "./switch";
export * from "./toggle-group";
