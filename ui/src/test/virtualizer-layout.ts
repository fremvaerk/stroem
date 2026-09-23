import { afterAll, beforeAll, vi } from "vitest";

/**
 * jsdom has no layout: give every element a 20px box so the virtualiser
 * can measure rows (the TanStack Virtual testing advice). The probe span's
 * char-width measurement reads getBoundingClientRect; @tanstack/react-virtual's
 * real item/container measurement (`measureElement`'s default implementation,
 * and the scroll container's observeElementRect) reads offsetHeight/offsetWidth
 * instead — jsdom leaves both at 0, so both need a fixed 20x800 box or the
 * virtualiser's first post-mount measurement pass zeroes every item's size.
 *
 * Call this at the top level of any test file that renders `LogViewer` and
 * asserts on row content.
 */
export function useVirtualizerLayout(): void {
  const originalOffsetHeight = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "offsetHeight");
  const originalOffsetWidth = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "offsetWidth");
  beforeAll(() => {
    vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(
      () => ({ x: 0, y: 0, top: 0, left: 0, bottom: 20, right: 800, width: 800, height: 20, toJSON: () => ({}) }) as DOMRect,
    );
    Object.defineProperty(HTMLElement.prototype, "offsetHeight", { configurable: true, value: 20 });
    Object.defineProperty(HTMLElement.prototype, "offsetWidth", { configurable: true, value: 800 });
  });
  afterAll(() => {
    vi.restoreAllMocks();
    if (originalOffsetHeight) Object.defineProperty(HTMLElement.prototype, "offsetHeight", originalOffsetHeight);
    if (originalOffsetWidth) Object.defineProperty(HTMLElement.prototype, "offsetWidth", originalOffsetWidth);
  });
}
