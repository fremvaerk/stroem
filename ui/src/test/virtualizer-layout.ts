import { afterAll, beforeAll, vi } from "vitest";

const BOX = { width: 800, height: 20 };

/**
 * jsdom has no layout: give every element an 800x20 box so the virtualiser
 * can measure rows (the TanStack Virtual testing advice). jsdom leaves all
 * of these at 0, and each one is read by something:
 * - the probe span's char-width measurement reads getBoundingClientRect;
 * - @tanstack/virtual-core's item measurement (`measureElement`'s default)
 *   reads offsetHeight/offsetWidth;
 * - its scroll-container measurement (`observeElementRect`) reads
 *   clientWidth/clientHeight since virtual-core 3.19 (offsetWidth/offsetHeight
 *   before). A zero container size gives an empty range, so no row renders.
 *
 * Call this at the top level of any test file that renders `LogViewer` and
 * asserts on row content.
 */
export function installVirtualizerLayout(): void {
  const sizes = {
    offsetHeight: BOX.height,
    offsetWidth: BOX.width,
    clientHeight: BOX.height,
    clientWidth: BOX.width,
  } as const;
  const originals = Object.fromEntries(
    Object.keys(sizes).map((key) => [key, Object.getOwnPropertyDescriptor(HTMLElement.prototype, key)]),
  );
  beforeAll(() => {
    vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(
      () =>
        ({
          x: 0,
          y: 0,
          top: 0,
          left: 0,
          bottom: BOX.height,
          right: BOX.width,
          width: BOX.width,
          height: BOX.height,
          toJSON: () => ({}),
        }) as DOMRect,
    );
    for (const [key, value] of Object.entries(sizes)) {
      Object.defineProperty(HTMLElement.prototype, key, { configurable: true, value });
    }
  });
  afterAll(() => {
    vi.restoreAllMocks();
    for (const [key, original] of Object.entries(originals)) {
      if (original) Object.defineProperty(HTMLElement.prototype, key, original);
      else delete (HTMLElement.prototype as unknown as Record<string, unknown>)[key];
    }
  });
}
