'use strict'
// Next's `cacheHandler` (ISR pages, route handlers, optimized images, `fetch`) on roll's shared
// cache. An entry is Next's value as JSON, its Buffers and Maps kept by tagging them.
const { scope, keyOf, get, put, mark } = require('./client.js')

function encode(value) {
  return Buffer.from(
    JSON.stringify(value, function (k, v) {
      const o = this[k]
      if (Buffer.isBuffer(o) || o instanceof Uint8Array) return { $b: Buffer.from(o).toString('base64') }
      if (o instanceof Map) return { $m: [...o] }
      return v
    }),
  )
}

function decode(bytes) {
  return JSON.parse(bytes.toString('utf8'), (k, v) => {
    if (v && typeof v === 'object' && !Array.isArray(v)) {
      const keys = Object.keys(v)
      if (keys.length === 1 && typeof v.$b === 'string') return Buffer.from(v.$b, 'base64')
      if (keys.length === 1 && Array.isArray(v.$m)) return new Map(v.$m)
    }
    return v
  })
}

const isFetch = (kind) => String(kind || '').toUpperCase() === 'FETCH'

// The build's prerendered pages, read by Next's own reader from its output (`.next/server`), so
// their format stays Next's: none without Next's context (a build), or if the reader moved.
function buildOutput(ctx) {
  if (!ctx || !ctx.fs || !ctx.serverDistDir) return null
  try {
    const FileSystemCache = require('next/dist/server/lib/incremental-cache/file-system-cache.js').default
    return new FileSystemCache({ ...ctx, flushToDisk: false, maxMemoryCacheSize: 0 })
  } catch {
    return null
  }
}

module.exports = class RollCacheHandler {
  constructor(ctx) {
    this.built = buildOutput(ctx)
  }

  async get(key, ctx) {
    const fetchCache = isFetch(ctx && ctx.kind)
    const soft = [...((ctx && ctx.tags) || []), ...((ctx && ctx.softTags) || [])]
    const hit = await get(scope(fetchCache), keyOf(fetchCache ? 'f' : 'p', key), soft)
    if (!hit) return fetchCache ? null : this.prerendered(key, ctx)
    try {
      // A tag made it stale: as old as can be, so Next serves it and renders it again behind it.
      return { lastModified: hit.stale ? 1 : hit.written, value: decode(hit.bytes) }
    } catch {
      return null
    }
  }

  async set(key, data, ctx) {
    if (!data) return
    const fetchCache = isFetch(data.kind)
    const tags = [...((ctx && ctx.tags) || [])]
    const own = data.headers && data.headers['x-next-cache-tags']
    if (typeof own === 'string') tags.push(...own.split(','))
    const cc = ctx && ctx.cacheControl
    await put(scope(fetchCache), keyOf(fetchCache ? 'f' : 'p', key), encode(data), {
      tags,
      written: Date.now(),
      expire: cc && Number.isFinite(cc.expire) ? cc.expire : undefined,
    })
  }

  // A page the build prerendered and roll's cache doesn't have yet (the first request after a
  // deploy): served as Next would serve it from its own output, and put into roll's cache as it's
  // read, so no instance renders it once more.
  async prerendered(key, ctx) {
    if (!this.built) return null
    try {
      const found = await this.built.get(key, ctx)
      if (!found || !found.value) return null
      await this.set(key, found.value, { tags: (ctx && ctx.tags) || [], cacheControl: found.cacheControl })
      return { lastModified: found.lastModified, value: found.value }
    } catch {
      return null
    }
  }

  async revalidateTag(tags, durations) {
    await mark(tags, durations && durations.expire)
  }

  resetRequestCache() {}
}
