// Lets `node --test` run the dashboard's TypeScript modules as Next resolves them: the `@/`
// alias and extensionless relative imports. Node strips the types itself (no build step).
import { registerHooks } from "node:module";
import { pathToFileURL } from "node:url";

const src = pathToFileURL(new URL("../src/", import.meta.url).pathname).href;

registerHooks({
  resolve(specifier, context, next) {
    const spec = specifier.startsWith("@/") ? src + specifier.slice(2) : specifier;
    try {
      return next(spec, context);
    } catch (e) {
      if (e?.code !== "ERR_MODULE_NOT_FOUND" || !(spec.startsWith(".") || spec.startsWith("file:"))) throw e;
      for (const ext of [".ts", ".tsx", "/index.ts"]) {
        try {
          return next(spec + ext, context);
        } catch {}
      }
      throw e;
    }
  },
});
