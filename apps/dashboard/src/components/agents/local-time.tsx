import { useSyncExternalStore } from "react";

const never = () => () => {};

/**
 * A time in the viewer's own zone and locale. A page rendered on the server (a share) would show
 * the server's zone and then fail to hydrate where it differs ("02:31 AM" vs "03:31"): it's empty
 * there and while hydrating, then the browser's.
 */
export function LocalTime({ at, format }: { at: number; format: (ms: number) => string }) {
  const here = useSyncExternalStore(never, () => true, () => false);
  return <>{here ? format(at) : ""}</>;
}
