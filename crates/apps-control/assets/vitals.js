// roll apps' Web Vitals (docs/apps/README.md, "Web Vitals"): what this page load measured, sent
// once when the page is hidden to /__roll/vitals on its own host. The browser's own
// PerformanceObserver only; no cookie, no id, no third party. LCP, CLS (the largest session
// window), INP (the slowest interaction), FCP and TTFB, in milliseconds (CLS unitless).
(() => {
  if (typeof PerformanceObserver !== "function") return;
  const v = {};
  const observe = (type, each, opts) => {
    try {
      new PerformanceObserver((list) => list.getEntries().forEach(each)).observe({ type, buffered: true, ...opts });
    } catch {
      // this browser doesn't report it
    }
  };
  observe("largest-contentful-paint", (e) => {
    v.lcp = e.startTime;
  });
  observe("paint", (e) => {
    if (e.name === "first-contentful-paint") v.fcp = e.startTime;
  });
  let win = 0;
  let first = 0;
  let last = 0;
  observe("layout-shift", (e) => {
    if (e.hadRecentInput) return;
    if (win && e.startTime - last < 1000 && e.startTime - first < 5000) win += e.value;
    else {
      win = e.value;
      first = e.startTime;
    }
    last = e.startTime;
    v.cls = Math.max(v.cls || 0, win);
  });
  observe(
    "event",
    (e) => {
      if (e.interactionId) v.inp = Math.max(v.inp || 0, e.duration);
    },
    { durationThreshold: 16 },
  );
  const nav = performance.getEntriesByType && performance.getEntriesByType("navigation")[0];
  if (nav) v.ttfb = Math.max(0, nav.responseStart - (nav.activationStart || 0));
  let sent = false;
  const send = () => {
    if (sent) return;
    sent = true;
    const body = JSON.stringify({ path: location.pathname, ...v });
    if (navigator.sendBeacon) navigator.sendBeacon("/__roll/vitals", body);
    else fetch("/__roll/vitals", { method: "POST", body, keepalive: true }).catch(() => {});
  };
  addEventListener("visibilitychange", () => document.visibilityState === "hidden" && send(), true);
  addEventListener("pagehide", send, true);
})();
