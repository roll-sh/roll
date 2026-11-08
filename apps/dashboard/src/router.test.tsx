import { describe, expect, it } from "vitest";
import { getRouter } from "./router";

const router = getRouter();
const route = (path: string) => router.getMatchedRoutes(path)[2]?.id;

describe("the route tree", () => {
  it("serves owner and repo pages, with the app's own routes first", () => {
    expect(route("/acme")).toBe("/$owner/");
    expect(route("/acme/web")).toBe("/$owner/$repo/");
    expect(route("/acme/web/tree/main")).toBe("/$owner/$repo/tree/$rev/$");
    expect(route("/acme/web/tree/main/src")).toBe("/$owner/$repo/tree/$rev/$");
    expect(route("/acme/web/blob/main/src/a.ts")).toBe("/$owner/$repo/blob/$rev/$");
    expect(router.getMatchedRoutes("/acme/web/blob/main/src/a.ts")[1]).toMatchObject({ rev: "main", _splat: "src/a.ts" });
    expect(route("/acme/web/changes/12")).toBe("/$owner/$repo/changes/$number");
    expect(route("/dashboard")).toBe("/dashboard/");
    expect(route("/dashboard/code/acme/web")).toBe("/dashboard/code/$owner/$repo/");
    expect(route("/dashboard/code/acme/web/changes")).toBe("/dashboard/code/$owner/$repo/$tab");
    expect(route("/dashboard/code/acme/web/changes/12")).toBe("/dashboard/code/$owner/$repo/changes/$number");
    expect(route("/dashboard/storage/-/iam")).toBe("/dashboard/storage/-/iam");
    expect(route("/dashboard/storage/photos")).toBe("/dashboard/storage/$bucket");
    expect(route("/agents/s/shr_1")).toBe("/agents/s/$id/");
    expect(route("/agents/s/shr_1/opengraph-image")).toBe("/agents/s/$id/opengraph-image");
  });

  it("sends git traffic to the git server routes", () => {
    expect(route("/acme/web.git/info/refs")).toBe("/$owner/$repo/info/refs");
    expect(route("/acme/web/info/refs")).toBe("/$owner/$repo/info/refs");
    expect(route("/acme/web.git/git-upload-pack")).toBe("/$owner/$repo/git-upload-pack");
    expect(route("/acme/web/git-receive-pack")).toBe("/$owner/$repo/git-receive-pack");
  });

  it("keeps search params as they're written", () => {
    for (const s of ["?demo", "?q=a%20b", "?vhost=%2F", "?state=landed&x=1"]) expect(router.options.stringifySearch(router.options.parseSearch(s))).toBe(s);
  });
});
