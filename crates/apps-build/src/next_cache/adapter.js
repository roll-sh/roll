'use strict'
// roll's Next.js adapter (NEXT_ADAPTER_PATH; Next 16 runs `modifyConfig` for `next build` and
// `next start`): Next's caches on roll's shared one, unless the app set its own handler or
// ROLL_NEXT_CACHE=off. The handlers are copied into the project's `.roll-next` first: Next's
// bundler and file tracing take a handler only from inside the project (so a standalone build
// carries them too).
const fs = require('node:fs')
const path = require('node:path')

const FILES = ['client.js', 'incremental.js', 'use-cache.js']

function place(projectDir) {
  const dir = path.join(projectDir || process.cwd(), '.roll-next')
  try {
    fs.mkdirSync(dir, { recursive: true })
    for (const f of FILES) {
      const text = fs.readFileSync(path.join(__dirname, f))
      const to = path.join(dir, f)
      if (!fs.existsSync(to) || !fs.readFileSync(to).equals(text)) fs.writeFileSync(to, text)
    }
    return dir
  } catch {
    // A read-only image at `next start`: the copy the build made, if it's there.
    return FILES.every((f) => fs.existsSync(path.join(dir, f))) ? dir : null
  }
}

module.exports = {
  name: 'roll',
  async modifyConfig(config, ctx) {
    if (process.env.ROLL_NEXT_CACHE === 'off') return config
    const dir = place(ctx && ctx.projectDir)
    if (!dir) return config
    const own = config.cacheHandlers || {}
    return {
      ...config,
      cacheHandler: config.cacheHandler || path.join(dir, 'incremental.js'),
      cacheHandlers: {
        ...own,
        default: own.default || path.join(dir, 'use-cache.js'),
        remote: own.remote || path.join(dir, 'use-cache.js'),
      },
    }
  },
}
