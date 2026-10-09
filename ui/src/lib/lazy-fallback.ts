import type { ComponentType } from "react";

/**
 * A `React.lazy` loader that settles on `fallback` when the chunk cannot be
 * fetched — e.g. a tab opened before a server upgrade asks for an asset hash
 * the new binary no longer embeds.
 */
export function importOr<P>(
  load: () => Promise<{ default: ComponentType<P> }>,
  fallback: ComponentType<P>,
): Promise<{ default: ComponentType<P> }> {
  return load().catch((err: unknown) => {
    console.warn("Falling back after a failed chunk load:", err);
    return { default: fallback };
  });
}
