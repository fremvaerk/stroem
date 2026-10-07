import { describe, it, expect } from "vitest";
import {
  initialJsonFieldState,
  parseJsonText,
  valueNotes,
  hasMask,
  hasTemplate,
} from "../json-field";
import type { InputField } from "../types";

const plain: InputField = { type: "json", default: { a: 1 } };
const templated: InputField = { type: "json", default: { host: "{{ secret.H }}" } };
const bare: InputField = { type: "json" };

describe("initialJsonFieldState (spec § 7, first match wins)", () => {
  it("replays a masked source value whole", () => {
    expect(initialJsonFieldState(templated, { value: { t: "x••••••" } }).mode).toBe("replay");
  });
  it("re-run with a masked source AND a templated default still replays (source wins)", () => {
    expect(initialJsonFieldState(templated, { value: ["••••••"] }).mode).toBe("replay");
  });
  it("replays an unmasked source value too (exact server-side replay)", () => {
    expect(initialJsonFieldState(templated, { value: { k: 2 } })).toEqual({ kind: "json", mode: "replay", text: "" });
  });
  it("re-run with a source value equal to the default replays", () => {
    expect(initialJsonFieldState(plain, { value: { a: 1 } }).mode).toBe("replay");
  });
  it("uses default mode for a templated default when the source lacks the field", () => {
    expect(initialJsonFieldState(templated).mode).toBe("default");
  });
  it("prefills an untemplated default in value mode", () => {
    expect(initialJsonFieldState(plain).text).toBe(JSON.stringify({ a: 1 }, null, 2));
  });
  it("starts empty without a default", () => {
    expect(initialJsonFieldState(bare)).toEqual({ kind: "json", mode: "value", text: "" });
  });
});

describe("helpers", () => {
  it("detects masks and templates at any depth", () => {
    expect(hasMask({ a: [{ b: "••••••" }] })).toBe(true);
    expect(hasMask({ a: 1 })).toBe(false);
    expect(hasTemplate(["{% if x %}"])).toBe(true);
    expect(hasTemplate("plain")).toBe(false);
  });
  it("parses JSON or reports Invalid JSON", () => {
    expect(parseJsonText('{"a": [1]}')).toEqual({ ok: true, value: { a: [1] } });
    const bad = parseJsonText('{"a": }');
    expect(bad.ok).toBe(false);
    if (!bad.ok) expect(bad.error.startsWith("Invalid JSON")).toBe(true);
  });
  it("notes masked and template text without blocking", () => {
    expect(valueNotes({ a: "••••••" })).toHaveLength(1);
    expect(valueNotes({ a: "{{ x }}" })).toHaveLength(1);
    expect(valueNotes({ a: 1 })).toEqual([]);
  });
});
