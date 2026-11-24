// scripts/preview-cost.sh: the preview's feedback button driven in Chromium. It opens $LINK (a
// preview's link with its access token), picks the page's first link, writes a note and posts it;
// roll's dashboard isn't opened (window.open is caught), and what it would open is printed as
// "<fb_path> · <fb_selector> · <fb_note>".
const { chromium } = require("playwright-core");
const fs = require("fs");
(async () => {
  const exe = fs.readdirSync("/ms-playwright").filter((d) => d.startsWith("chromium_headless_shell"))[0];
  const b = await chromium.launch({ executablePath: `/ms-playwright/${exe}/chrome-headless-shell-linux64/chrome-headless-shell` });
  try {
    const p = await b.newPage();
    // The toolbar's shadow root is closed: opened for this check only.
    await p.addInitScript(() => {
      window.open = (u) => {
        window.__opened = u;
      };
      const o = Element.prototype.attachShadow;
      Element.prototype.attachShadow = function (i) {
        return o.call(this, { ...i, mode: "open" });
      };
    });
    await p.goto(process.env.LINK, { waitUntil: "load" });
    const bar = p.locator("[data-roll-feedback]");
    await bar.locator("button.btn").click();
    await p.locator("a").first().click();
    await bar.locator("form.card textarea").fill("a live note");
    await bar.locator("form.card button.go").click();
    const u = new URL(await p.evaluate(() => window.__opened || ""));
    console.log([u.searchParams.get("fb_path"), u.searchParams.get("fb_selector"), u.searchParams.get("fb_note")].join(" · "));
  } catch (e) {
    console.log(`failed: ${e.message.split("\n")[0]}`);
  } finally {
    await b.close();
  }
})();
