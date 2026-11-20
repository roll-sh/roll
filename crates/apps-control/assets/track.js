// roll apps' custom events (docs/apps/README.md, "Visitors"): `roll.track(name, props)` sends one
// event to /__roll/event on this page's own host, with the page's path. No cookie, no id, no
// third party. Properties are at most ten strings, numbers or booleans.
(() => {
  const roll = (window.roll = window.roll || {});
  roll.track = (name, props) => {
    const body = JSON.stringify({ name: String(name), props: props || undefined, path: location.pathname });
    if (navigator.sendBeacon && navigator.sendBeacon("/__roll/event", new Blob([body], { type: "application/json" }))) return;
    fetch("/__roll/event", { method: "POST", body, headers: { "content-type": "application/json" }, keepalive: true }).catch(() => {});
  };
})();
