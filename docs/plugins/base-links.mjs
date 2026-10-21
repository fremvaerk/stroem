// Astro applies `base` to its own routes and to Starlight sidebar links, but
// not to links inside Markdown. This Sätteri mdast plugin prefixes the base to
// every root-relative link, so content links are written as `/guides/hooks/`.

export function withBase(url, base) {
  if (!base || !url.startsWith("/") || url.startsWith("//")) return url;
  const rest = url.slice(base.length);
  if (url.startsWith(base) && (rest === "" || /^[/?#]/.test(rest))) return url;
  return base + url;
}

function normalizeBase(base) {
  return (base ?? "").replace(/\/+$/, "");
}

export default function baseLinks(base) {
  const prefix = normalizeBase(base);
  if (!prefix) return null;
  const rewrite = (node, ctx) => {
    const url = withBase(node.url, prefix);
    if (url !== node.url) ctx.setProperty(node, "url", url);
  };
  return { name: "stroem-base-links", link: rewrite, definition: rewrite };
}
