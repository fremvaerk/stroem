import type { ComponentType } from "react";
import { describe, expect, it } from "vitest";
import { importOr } from "./lazy-fallback";

type Loaded = { default: ComponentType<object> };
const Real: ComponentType<object> = () => null;
const Fallback: ComponentType<object> = () => null;

describe("importOr", () => {
  it("resolves the imported module", async () => {
    const mod = await importOr(() => Promise.resolve<Loaded>({ default: Real }), Fallback);
    expect(mod.default).toBe(Real);
  });

  it("falls back when the chunk cannot be loaded", async () => {
    const mod = await importOr(
      () => Promise.reject<Loaded>(new TypeError("Failed to fetch dynamically imported module")),
      Fallback,
    );
    expect(mod.default).toBe(Fallback);
  });
});
