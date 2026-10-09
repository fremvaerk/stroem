import { describe, expect, it } from "vitest";
import { HIGHLIGHT_MAX_CHARS, highlightSegments } from "./code-language";

const join = (code: string) =>
  highlightSegments(code, "json")
    .map((s) => s.text)
    .join("");

const classOf = (code: string, text: string) =>
  highlightSegments(code, "json").find((s) => s.text === text)?.cls;

describe("highlightSegments", () => {
  it("is lossless for valid, partial and invalid input", () => {
    for (const code of [
      "",
      '{\n  "a": 1,\n  "b": [true, false, null],\n  "c": {"d": "e"}\n}',
      '{"a": "x\\"y\\u00e9", "k:v": -1.5e3}',
      '{"unterminated": "abc',
      "{",
      "}}} trailing garbage ,,, ",
      "\t\r\n  [1,2,3]  \n\n",
      '"just a string"',
    ]) {
      expect(join(code)).toBe(code);
    }
  });

  it("tells keys from string values", () => {
    const code = '{"name": "value"}';
    expect(classOf(code, '"name"')).toBe("tok-propertyName");
    expect(classOf(code, '"value"')).toBe("tok-string");
  });

  it("classes numbers, booleans, null and punctuation", () => {
    const code = '{"n": -1.5e3, "t": true, "z": null}';
    expect(classOf(code, "-1.5e3")).toBe("tok-number");
    expect(classOf(code, "true")).toBe("tok-bool");
    expect(classOf(code, "null")).toBe("tok-keyword");
    expect(classOf(code, "{")).toBe("tok-punctuation");
  });

  it("keeps a key containing a colon whole", () => {
    expect(classOf('{"k:v": 1}', '"k:v"')).toBe("tok-propertyName");
  });

  it("leaves whitespace unclassed", () => {
    const segs = highlightSegments('{\n  "a": 1\n}', "json");
    for (const s of segs.filter((s) => /^\s+$/.test(s.text))) {
      expect(s.cls).toBe("");
    }
  });

  it("returns one plain segment above the size cap", () => {
    const code = `"${"x".repeat(HIGHLIGHT_MAX_CHARS)}"`;
    expect(highlightSegments(code, "json")).toEqual([{ text: code, cls: "" }]);
  });
});
