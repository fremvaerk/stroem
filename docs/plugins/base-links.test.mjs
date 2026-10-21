import { describe, expect, test } from "bun:test";
import baseLinks, { withBase } from "./base-links.mjs";

function recorder() {
  const calls = [];
  return { calls, ctx: { setProperty: (node, key, value) => calls.push([node, key, value]) } };
}

describe("withBase", () => {
  test("prefixes a root-relative link", () => {
    expect(withBase("/guides/hooks/", "/stroem")).toBe("/stroem/guides/hooks/");
    expect(withBase("/guides/git-refs/#templated-refs", "/stroem")).toBe(
      "/stroem/guides/git-refs/#templated-refs",
    );
  });

  test("leaves a link that already has the base", () => {
    for (const url of ["/stroem", "/stroem/", "/stroem/guides/hooks", "/stroem#top", "/stroem?q=1"]) {
      expect(withBase(url, "/stroem")).toBe(url);
    }
  });

  test("prefixes a path that only starts with the base's letters", () => {
    expect(withBase("/stroemish/", "/stroem")).toBe("/stroem/stroemish/");
  });

  test("leaves non-root-relative links", () => {
    for (const url of [
      "https://github.com/fremvaerk/stroem",
      "//cdn.example.com/x.js",
      "#anchor",
      "../hooks/",
      "hooks/",
      "mailto:a@example.com",
      "",
    ]) {
      expect(withBase(url, "/stroem")).toBe(url);
    }
  });
});

describe("baseLinks", () => {
  test("rewrites link and definition nodes", () => {
    const plugin = baseLinks("/stroem");
    const { calls, ctx } = recorder();
    const link = { type: "link", url: "/guides/retry/", children: [] };
    const def = { type: "definition", identifier: "h", url: "/guides/hooks/" };
    plugin.link(link, ctx);
    plugin.definition(def, ctx);
    expect(calls).toEqual([
      [link, "url", "/stroem/guides/retry/"],
      [def, "url", "/stroem/guides/hooks/"],
    ]);
  });

  test("does not touch a link that needs no change", () => {
    const plugin = baseLinks("/stroem");
    const { calls, ctx } = recorder();
    for (const url of ["/stroem/guides/retry/", "https://example.com/", "../hooks/"]) {
      plugin.link({ type: "link", url, children: [] }, ctx);
    }
    expect(calls).toEqual([]);
  });

  test("visits only link and definition nodes", () => {
    expect(Object.keys(baseLinks("/stroem")).sort()).toEqual(["definition", "link", "name"]);
  });

  test("ignores a trailing slash on the base", () => {
    const { calls, ctx } = recorder();
    baseLinks("/stroem/").link({ type: "link", url: "/guides/retry/", children: [] }, ctx);
    expect(calls[0][2]).toBe("/stroem/guides/retry/");
  });

  test("is left out of the pipeline without a base", () => {
    for (const base of [undefined, "", "/"]) {
      expect(baseLinks(base)).toBeNull();
    }
  });
});
