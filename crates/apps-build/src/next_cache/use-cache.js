'use strict'
// Next's `cacheHandlers` (`"use cache"`, `"use cache: remote"`) on roll's shared cache. An entry is
// a 4-byte length, its fields as JSON (tags, stale, timestamp, expire, revalidate), then its value.
// Soft tags (a path's, for `revalidatePath`) are checked by roll on each read.
const { scope, keyOf, get, put, mark } = require('./client.js')

const pending = new Map()
const num = (v) => (typeof v === 'number' ? v : Infinity)

function stream(bytes) {
  return new ReadableStream({
    start(controller) {
      controller.enqueue(new Uint8Array(bytes))
      controller.close()
    },
  })
}

module.exports = {
  async get(cacheKey, softTags) {
    const waiting = pending.get(cacheKey)
    if (waiting) await waiting
    const hit = await get(scope(false), keyOf('u', cacheKey), softTags)
    if (!hit || hit.bytes.length < 4) return undefined
    try {
      const n = hit.bytes.readUInt32BE(0)
      const m = JSON.parse(hit.bytes.subarray(4, 4 + n).toString('utf8'))
      return {
        value: stream(hit.bytes.subarray(4 + n)),
        tags: Array.isArray(m.tags) ? m.tags : [],
        stale: num(m.stale),
        timestamp: num(m.timestamp),
        expire: num(m.expire),
        // A tag made it stale: served, and made again behind it.
        revalidate: hit.stale ? -1 : num(m.revalidate),
      }
    } catch {
      return undefined
    }
  },

  async set(cacheKey, pendingEntry) {
    let done
    pending.set(cacheKey, new Promise((resolve) => (done = resolve)))
    try {
      const entry = await pendingEntry
      const value = Buffer.from(await new Response(entry.value).arrayBuffer())
      const fields = Buffer.from(
        JSON.stringify({ tags: entry.tags, stale: entry.stale, timestamp: entry.timestamp, expire: entry.expire, revalidate: entry.revalidate }),
      )
      const head = Buffer.alloc(4)
      head.writeUInt32BE(fields.length, 0)
      await put(scope(false), keyOf('u', cacheKey), Buffer.concat([head, fields, value]), {
        tags: entry.tags,
        written: entry.timestamp,
        expire: entry.expire,
      })
    } catch {
      // An interrupted value isn't kept: an incomplete page must never be served.
    } finally {
      pending.delete(cacheKey)
      done()
    }
  },

  async refreshTags() {},

  // Soft tags are roll's to check, on each read.
  async getExpiration() {
    return Infinity
  },

  async updateTags(tags, durations) {
    await mark(tags, durations && durations.expire)
  },
}
