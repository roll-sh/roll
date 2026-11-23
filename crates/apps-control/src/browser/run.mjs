// roll's browser check runner (crates/apps-control/src/browser.rs): one Chromium page at
// ROLL_CHECK_URL, then the check's own script, the body of an async function given `page`,
// `context` and `url`. The environment's host is reached through roll (ROLL_CHECK_PROXY: its
// requests are fetched there by Playwright, every other host directly by the browser), carrying
// ROLL_CHECK_TOKEN so the environment's gates let it in as they let roll's uptime checks. The
// last line is the result: `ROLL-CHECK {"ok":…,"status":…,"error":…,"ms":…}`.
import { chromium } from '/roll-check/node_modules/playwright-core/index.mjs'

const env = process.env
const url = env.ROLL_CHECK_URL
const target = new URL(url)
const timeout = Number(env.ROLL_CHECK_TIMEOUT_MS) || 60000
const script = Buffer.from(env.ROLL_CHECK_SCRIPT || '', 'base64').toString('utf8')
const started = Date.now()
const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor

function say(result) {
  console.log('ROLL-CHECK ' + JSON.stringify({ ...result, ms: Date.now() - started }))
}

let browser
let status = null
let code = 0
try {
  browser = await chromium.launch({ timeout })
  const context = await browser.newContext({ userAgent: 'Mozilla/5.0 (X11; Linux x86_64) roll-browser-check/1.0 (Playwright)' })
  context.setDefaultTimeout(timeout)
  // The environment's own host through roll, fetched here (never by the browser, so neither its
  // address nor the token is the page's): every other host as the browser reaches it.
  if (env.ROLL_CHECK_PROXY) {
    await context.route((u) => u.host === target.host, async (route) => {
      const u = new URL(route.request().url())
      const response = await route.fetch({
        url: `http://${env.ROLL_CHECK_PROXY}${u.pathname}${u.search}`,
        headers: { ...route.request().headers(), host: target.host, 'x-roll-check': env.ROLL_CHECK_TOKEN || '' },
        maxRedirects: 0,
      })
      await route.fulfill({ response })
    })
  }
  const page = await context.newPage()
  const resp = await page.goto(url, { timeout, waitUntil: 'load' })
  status = resp ? resp.status() : null
  const want = Number(env.ROLL_CHECK_STATUS) || 0
  if (want ? status !== want : !(status >= 200 && status < 400)) throw new Error(`answered ${status}`)
  if (env.ROLL_CHECK_CONTAINS && !(await page.content()).includes(env.ROLL_CHECK_CONTAINS)) throw new Error(`the page doesn't say "${env.ROLL_CHECK_CONTAINS}"`)
  if (script.trim()) await new AsyncFunction('page', 'context', 'url', script)(page, context, url)
  say({ ok: true, status })
} catch (e) {
  code = 1
  // Its first line, and what it was waiting for from Playwright's call log.
  const lines = String((e && e.message) || e).split('\n')
  const waiting = lines.find((l) => l.trim().startsWith('- waiting for '))
  say({ ok: false, status, error: (waiting ? `${lines[0]} (${waiting.trim().slice(2)})` : lines[0]).slice(0, 300) })
} finally {
  await browser?.close().catch(() => {})
}
process.exit(code)
