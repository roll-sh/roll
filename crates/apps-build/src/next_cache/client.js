'use strict'
// roll's Next.js cache client (docs/apps/README.md, "Next.js across instances"): entries and tag
// invalidations on roll's apps listener, shared by every instance of the environment. Off (every
// read a miss, every write dropped) without ROLL_NEXT_CACHE_URL, ROLL_NEXT_CACHE_TOKEN and
// ROLL_DEPLOYMENT (a build, `next dev`), or with ROLL_NEXT_CACHE=off. A failure is a miss: the
// page renders as it would with no cache.
//
// node:http, never the global fetch: Next patches that one, and with Cache Components a fetch it
// sees inside a render can be held forever (it's taken for the render's own I/O).
const crypto = require('node:crypto')
const http = require('node:http')
const https = require('node:https')

const BASE = (process.env.ROLL_NEXT_CACHE_URL || '').replace(/\/+$/, '')
const TOKEN = process.env.ROLL_NEXT_CACHE_TOKEN || ''
const DEPLOYMENT = (process.env.ROLL_DEPLOYMENT || '').replace(/[^A-Za-z0-9_-]/g, '').slice(0, 64)
const on = Boolean(BASE && TOKEN && DEPLOYMENT) && process.env.ROLL_NEXT_CACHE !== 'off'
const agents = { 'http:': new http.Agent({ keepAlive: true }), 'https:': new https.Agent({ keepAlive: true }) }

// Next's fetch cache is data every deployment may share; everything else is this deployment's.
function scope(fetchCache) {
  return fetchCache ? 'data' : 'd.' + DEPLOYMENT
}

function keyOf(ns, key) {
  return crypto.createHash('sha256').update(ns + '\0' + key).digest('hex')
}

function tagList(tags) {
  const out = []
  for (const t of tags || []) {
    if (typeof t === 'string' && t.length > 0 && Buffer.byteLength(t) <= 256 && !out.includes(t)) out.push(t)
    if (out.length === 128) break
  }
  return out
}

// One request to roll: its status, headers and body, or a rejection after `ms`.
function request(method, path, headers, body, ms) {
  return new Promise((resolve, reject) => {
    const url = new URL(BASE + path)
    const lib = url.protocol === 'https:' ? https : http
    const req = lib.request(url, { method, agent: agents[url.protocol], headers: { authorization: 'Bearer ' + TOKEN, ...headers } }, (res) => {
      const chunks = []
      res.on('data', (c) => chunks.push(c))
      res.on('end', () => resolve({ status: res.statusCode, headers: res.headers, body: Buffer.concat(chunks) }))
      res.on('error', reject)
    })
    req.setTimeout(ms, () => req.destroy(new Error('roll: the cache took too long')))
    req.on('error', reject)
    req.end(body)
  })
}

// The entry's bytes, when it's there and no tag expired it; `stale` when a tag made it stale.
async function get(scopeName, key, softTags) {
  if (!on) return undefined
  try {
    const r = await request('GET', `/${scopeName}/${key}`, { 'x-roll-soft-tags': encodeURIComponent(JSON.stringify(tagList(softTags))) }, undefined, 5000)
    if (r.status !== 200) return undefined
    return { bytes: r.body, written: Number(r.headers['x-roll-written']) || 0, stale: r.headers['x-roll-stale'] === '1' }
  } catch {
    return undefined
  }
}

async function put(scopeName, key, bytes, { tags, written, expire }) {
  if (!on) return
  const h = {
    'content-type': 'application/octet-stream',
    'content-length': String(bytes.length),
    'x-roll-tags': encodeURIComponent(JSON.stringify(tagList(tags))),
    'x-roll-written': String(Math.floor(written || Date.now())),
  }
  if (Number.isFinite(expire) && expire > 0) h['x-roll-expire'] = String(Math.ceil(expire))
  try {
    await request('PUT', `/${scopeName}/${key}`, h, bytes, 30000)
  } catch {}
}

// Tags invalidated: expired now, or (`expire`, seconds) stale now and expired that much later.
async function mark(tags, expire) {
  if (!on) return
  const list = (Array.isArray(tags) ? tags : [tags]).filter((t) => typeof t === 'string' && t.length > 0)
  for (let i = 0; i < list.length; i += 128) {
    const body = { tags: tagList(list.slice(i, i + 128)) }
    if (Number.isFinite(expire)) body.expire = expire
    const text = Buffer.from(JSON.stringify(body))
    try {
      await request('POST', '/tags', { 'content-type': 'application/json', 'content-length': String(text.length) }, text, 5000)
    } catch {}
  }
}

module.exports = { on, scope, keyOf, get, put, mark }
