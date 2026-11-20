/**
 * A one-time share's page on the server (`/share/<id>`): what the link says before it's opened, read
 * anonymously and fresh, so an opened, expired or revoked share is a 404 at once. Reading it uses no
 * open up; only the page's button does (a `POST` from the browser).
 */

import { createServerFn } from "@tanstack/react-start";
import { serverApi } from "./roll-api";
import { peekRequest, RequestGone, type ValuePeek } from "./secrets-ask";
import { peekShare, ShareGone, type SharePeek } from "./secrets-share";

const peek = createServerFn({ method: "GET" })
  .validator((id: string) => id)
  .handler(async ({ data: id }): Promise<{ peek: SharePeek | null; now: number }> => {
    const now = Date.now();
    if (!/^[A-Za-z0-9_-]{1,64}$/.test(id)) return { peek: null, now };
    try {
      return { peek: await peekShare(id, serverApi()), now };
    } catch (e) {
      if (e instanceof ShareGone) return { peek: null, now };
      throw e;
    }
  });

/** The share's note, opens left and expiry, and the server's clock; `peek` is null for none. */
export const loadSharePeek = (id: string) => peek({ data: id });

const peekAsk = createServerFn({ method: "GET" })
  .validator((token: string) => token)
  .handler(async ({ data: token }): Promise<{ peek: ValuePeek | null; gone: boolean; now: number }> => {
    const now = Date.now();
    if (!/^rlq_[a-z2-7]{40}$/.test(token)) return { peek: null, gone: false, now };
    try {
      return { peek: await peekRequest(token, serverApi()), gone: false, now };
    } catch (e) {
      if (e instanceof RequestGone) return { peek: null, gone: e.status === 410, now };
      throw e;
    }
  });

/** A secret request's page (`/request/<token>`): its note, key names and public key, read anonymously and fresh. `gone`: it existed and can't be answered now. */
export const loadValueRequestPeek = (token: string) => peekAsk({ data: token });
