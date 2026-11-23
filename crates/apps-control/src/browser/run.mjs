// roll's browser check runner (crates/apps-control/src/browser.rs): one Chromium page at
// ROLL_CHECK_URL, then the check's own script, the body of an async function given `page`,
// `context` and `url`, and its lab metrics (LCP, CLS, TTFB, page weight, requests) against
// ROLL_CHECK_BUDGETS. The environment's host is reached through roll (ROLL_CHECK_PROXY: its
// requests are fetched there by Playwright, every other host directly by the browser), carrying
// ROLL_CHECK_TOKEN so the environment's gates let it in as they let roll's uptime checks. The
// last line is the result: `ROLL-CHECK {"ok":…,"status":…,"error":…,"ms":…}`.
import { mkdirSync } from 'node:fs'
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

// A failure's screenshot and trace go here; roll copies them out after the run.
const KEEP = '/tmp/roll-check'
let browser
let context
let page
let status = null
let code = 0
let metrics = null
const budgets = JSON.parse(env.ROLL_CHECK_BUDGETS || '{}')
let requests = 0
let bytes = 0
let pending = []
try {
  browser = await chromium.launch({ timeout })
  context = await browser.newContext({ userAgent: 'Mozilla/5.0 (X11; Linux x86_64) roll-browser-check/1.0 (Playwright)' })
  context.setDefaultTimeout(timeout)
  await context.tracing.start({ screenshots: true, snapshots: true })
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
  // Largest contentful paint and layout shifts as the page records them (Chromium's own entries).
  await context.addInitScript(() => {
    const m = (window.__roll = { lcp: 0, cls: 0 })
    new PerformanceObserver((l) => {
      for (const e of l.getEntries()) m.lcp = e.renderTime || e.loadTime || e.startTime
    }).observe({ type: 'largest-contentful-paint', buffered: true })
    new PerformanceObserver((l) => {
      for (const e of l.getEntries()) if (!e.hadRecentInput) m.cls += e.value
    }).observe({ type: 'layout-shift', buffered: true })
  })
  page = await context.newPage()
  // Its weight: every response's body as the page got it.
  page.on('response', (r) => {
    requests += 1
    pending.push(r.body().then((b) => (bytes += b.length), () => {}))
  })
  const resp = await page.goto(url, { timeout, waitUntil: 'load' })
  status = resp ? resp.status() : null
  const want = Number(env.ROLL_CHECK_STATUS) || 0
  if (want ? status !== want : !(status >= 200 && status < 400)) throw new Error(`answered ${status}`)
  if (env.ROLL_CHECK_CONTAINS && !(await page.content()).includes(env.ROLL_CHECK_CONTAINS)) throw new Error(`the page doesn't say "${env.ROLL_CHECK_CONTAINS}"`)
  if (script.trim()) await new AsyncFunction('page', 'context', 'url', script)(page, context, url)
  // A moment for the last paint to be the largest, then what the page measured.
  await page.waitForTimeout(500)
  await Promise.all(pending)
  const m = await page.evaluate(() => ({ lcp: window.__roll?.lcp || 0, cls: window.__roll?.cls || 0, ttfb: performance.getEntriesByType('navigation')[0]?.responseStart || 0 }))
  metrics = { lcp_ms: Math.round(m.lcp), cls: Math.round(m.cls * 1000) / 1000, ttfb_ms: Math.round(m.ttfb), kb: Math.round(bytes / 1024), requests }
  const over = [
    ['lcp_ms', 'LCP', ' ms'],
    ['cls', 'CLS', ''],
    ['ttfb_ms', 'TTFB', ' ms'],
    ['kb', 'its weight', ' KiB'],
    ['requests', 'its requests', ''],
  ].filter(([k]) => typeof budgets[k] === 'number' && metrics[k] > budgets[k])
  if (over.length) throw new Error('over its budget: ' + over.map(([k, name, unit]) => `${name} ${metrics[k]}${unit} > ${budgets[k]}${unit}`).join(', '))
  await context.tracing.stop()
  say({ ok: true, status, metrics })
} catch (e) {
  code = 1
  // Its first line, and what it was waiting for from Playwright's call log.
  const lines = String((e && e.message) || e).split('\n')
  const waiting = lines.find((l) => l.trim().startsWith('- waiting for '))
  say({ ok: false, status, metrics, error: (waiting ? `${lines[0]} (${waiting.trim().slice(2)})` : lines[0]).slice(0, 300) })
  try {
    mkdirSync(KEEP, { recursive: true })
    if (page) await page.screenshot({ path: `${KEEP}/screenshot.png`, timeout: 5000 }).catch(() => {})
    if (context) await context.tracing.stop({ path: `${KEEP}/trace.zip` }).catch(() => {})
  } catch {}
} finally {
  await browser?.close().catch(() => {})
}
process.exit(code)
