import { describe, it, expect } from "vitest";
import { collectDependsOnNames, type DependsOnEntry } from "../types";

describe("collectDependsOnNames", () => {
  it("returns an empty array for no entries", () => {
    expect(collectDependsOnNames([])).toEqual([]);
  });

  it("passes through bare step names", () => {
    expect(collectDependsOnNames(["a", "b"])).toEqual(["a", "b"]);
  });

  it("extracts the step name from a {step, accept} entry", () => {
    const entries: DependsOnEntry[] = [{ step: "build", accept: ["failed"] }];
    expect(collectDependsOnNames(entries)).toEqual(["build"]);
  });

  it("walks into an any group", () => {
    const entries: DependsOnEntry[] = [{ any: ["a", "b"] }];
    expect(collectDependsOnNames(entries)).toEqual(["a", "b"]);
  });

  it("walks into an all group", () => {
    const entries: DependsOnEntry[] = [{ all: ["a", "b"] }];
    expect(collectDependsOnNames(entries)).toEqual(["a", "b"]);
  });

  it("walks a nested tree and preserves duplicates", () => {
    const entries: DependsOnEntry[] = [
      "a",
      { any: [{ step: "a" }, { all: ["b", "c"] }] },
    ];
    expect(collectDependsOnNames(entries)).toEqual(["a", "a", "b", "c"]);
  });
});
