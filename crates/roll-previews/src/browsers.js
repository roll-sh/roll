// roll previews' other browsers (docs/previews.md, "Surfaces"; `github.browsers`): the first page
// taken in Firefox and WebKit too, and in WebKit at a phone's size (mobile Safari's engine), as
// Percy and Chromatic do, with Playwright's builds in the screenshot image and `playwright-core`
// in the derived one (`runtime::BROWSERS_IMAGE`).
// Stilled as Chromium's capture is (animations and transitions off, the caret hidden, $HIDE
// blanked). One JSON line per browser on stdout: {browser, errors, png (base64)}; a browser that
// fails is left out. The page is the preview's code: roll bounds what it reads back.
// And the open notes' elements whose reviewer's browser was another engine ($CROPS: [{note,
// path, selector, width?, height?, steps?, browser}], checked by roll), each in that engine at
// their viewport after their clicks and keys, cropped with a margin: {crop: note, png}.
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
const CROPS = (() => {
  try {
    return JSON.parse(process.env.CROPS || "[]").filter((c) => c && typeof c.note === "string" && typeof c.path === "string" && typeof c.selector === "string" && ["firefox", "webkit", "webkit-phone"].includes(c.browser)).slice(0, 10);
  } catch {
    return [];
  }
})();
// A note's element in its reviewer's engine: at their viewport (a phone's for WebKit on iOS),
// after their clicks and keys (10 s at most; the first that finds nothing ends them).
const crop = async (c) => {
  let browser = null;
  try {
    const phone = c.browser === "webkit-phone";
    browser = await pw[phone ? "webkit" : c.browser].launch();
    const w = Number.isInteger(c.width) && c.width >= 320 && c.width <= 2560 ? c.width : phone ? 390 : 1280;
    const h = Number.isInteger(c.height) && c.height >= 320 && c.height <= 2000 ? c.height : phone ? 844 : 800;
    const page = await browser.newPage(phone ? { viewport: { width: w, height: h }, deviceScaleFactor: 2, isMobile: true, hasTouch: true } : { viewport: { width: w, height: h } });
    if (!/^\/(?!\/)[^\s#"'`\\]{0,199}$/.test(c.path)) return;
    await page.goto(new URL(c.path, process.env.URL).href, { waitUntil: "load", timeout: 30000 });
    await sleep(300);
    const until = Date.now() + 10000;
    for (const s of (Array.isArray(c.steps) ? c.steps : []).slice(0, 10)) {
      const left = Math.max(until - Date.now(), 0);
      if (!left) break;
      try {
        if (typeof s.click === "string") await page.click(s.click, { timeout: Math.min(left, 3000) });
        // Typing as long as theirs (never their words: roll types `x`s).
        else if (Array.isArray(s.type) && s.type.length === 2 && typeof s.type[0] === "string" && typeof s.type[1] === "string") await page.fill(s.type[0], s.type[1].slice(0, 200), { timeout: Math.min(left, 3000) });
        else if (s.press === "Enter" || s.press === "Escape") await page.keyboard.press(s.press);
        else break;
      } catch {
        break;
      }
      await sleep(150);
    }
    await sleep(300);
    await page.addStyleTag({ content: STILL }).catch(() => null);
    const el = page.locator(c.selector).first();
    await el.scrollIntoViewIfNeeded({ timeout: 3000 });
    const b = await el.boundingBox();
    if (!b || !(b.width > 0 && b.height > 0)) return;
    // A margin around it, within the page's width (a clip past it is refused), at most 1280 by
    // 800, from the whole page (it may sit below the fold).
    const m = 24;
    const x = Math.max(0, b.x - m), y = Math.max(0, b.y + (await page.evaluate(() => scrollY)) - m);
    const width = Math.max(1, Math.min(b.width + 2 * m, 1280, w - x));
    const png = await page.screenshot({ fullPage: true, clip: { x, y, width, height: Math.min(b.height + 2 * m, 800) } });
    if (png.length > 512 * 1024) return;
    process.stdout.write(JSON.stringify({ crop: c.note, png: png.toString("base64") }) + "\n");
  } catch (e) {
    console.error(`crop ${c.note}: ${e.message}`);
  } finally {
    if (browser) await browser.close().catch(() => null);
  }
};
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
      // Twice the density, as Chromium's phone screenshot: three times would pass roll's 2 MiB a picture on a page of photos.
      const page = await browser.newPage(phone ? { viewport: { width: 390, height: 844 }, deviceScaleFactor: 2, isMobile: true, hasTouch: true } : { viewport: { width: 1280, height: 800 } });
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
      // Past roll's limit a picture is dropped anyway: left out here, so the others still fit.
      if (png.length > 2 * 1024 * 1024) throw new Error("its screenshot is over 2 MiB");
      process.stdout.write(JSON.stringify({ browser: name, errors: Math.min(errors, 10000), png: png.toString("base64") }) + "\n");
    } catch (e) {
      console.error(`${name}: ${e.message}`);
    } finally {
      if (browser) await browser.close().catch(() => null);
    }
  }
  for (const c of CROPS) await crop(c);
})();
