// roll previews' other browsers (docs/previews.md, "Surfaces"; `github.browsers`): the first page
// taken in Firefox and WebKit too, and in WebKit at a phone's size (mobile Safari's engine), as
// Percy and Chromatic do, with Playwright's builds in the screenshot image and `playwright-core`
// in the derived one (`runtime::BROWSERS_IMAGE`).
// Stilled as Chromium's capture is (animations and transitions off, the caret hidden, $HIDE
// blanked). One JSON line per browser on stdout: {browser, errors, png (base64)}; a browser that
// fails is left out. The page is the preview's code: roll bounds what it reads back.
const pw = require("playwright-core");
const HIDE = (() => {
  try {
    return JSON.parse(process.env.HIDE || "[]").filter((s) => typeof s === "string").slice(0, 20);
  } catch {
    return [];
  }
})();
const STILL =
  "*,*::before,*::after{animation:none!important;transition:none!important;caret-color:transparent!important;scroll-behavior:auto!important}" +
  HIDE.map((x) => x + "{visibility:hidden!important}").join("");
const sleep = (ms) => new Promise((ok) => setTimeout(ok, ms));
(async () => {
  let browsers = [];
  try {
    browsers = JSON.parse(process.env.BROWSERS || "[]").filter((b) => b === "firefox" || b === "webkit" || b === "webkit-phone");
  } catch {}
  for (const name of browsers) {
    let browser = null;
    try {
      // `webkit-phone`: mobile Safari's engine at a phone's size and density, touch on.
      const phone = name === "webkit-phone";
      browser = await pw[phone ? "webkit" : name].launch();
      const page = await browser.newPage(phone ? { viewport: { width: 390, height: 844 }, deviceScaleFactor: 3, isMobile: true, hasTouch: true } : { viewport: { width: 1280, height: 800 } });
      let errors = 0;
      page.on("pageerror", () => errors++);
      page.on("console", (m) => m.type() === "error" && errors++);
      page.on("requestfailed", () => errors++);
      await page.goto(process.env.URL, { waitUntil: "load", timeout: 30000 });
      await sleep(300);
      await page.addStyleTag({ content: STILL }).catch(() => null);
      await page.evaluate(() => document.querySelectorAll("video").forEach((v) => { try { v.pause(); v.currentTime = 0; } catch {} })).catch(() => null);
      await sleep(100);
      const png = await page.screenshot();
      process.stdout.write(JSON.stringify({ browser: name, errors: Math.min(errors, 10000), png: png.toString("base64") }) + "\n");
    } catch (e) {
      console.error(`${name}: ${e.message}`);
    } finally {
      if (browser) await browser.close().catch(() => null);
    }
  }
})();
