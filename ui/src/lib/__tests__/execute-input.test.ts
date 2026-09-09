import { describe, it, expect } from "vitest";
import { buildExecuteInput } from "../execute-input";
import { SECRET_SENTINEL, REDACTED_SENTINEL } from "@/components/task/constants";
import type { InputField } from "../types";

const fields: Record<string, InputField> = {
  dataset: { type: "string" },
  day: { type: "date" },
  version: { type: "string", default: "" },
  env: { type: "string", default: "prod" },
  skip: { type: "boolean", default: false },
  limit: { type: "number" },
  count: { type: "number", default: 5 },
  token: { type: "string", secret: true, default: "abc" },
  clickhouse: { type: "clickhouse" },
};

describe("buildExecuteInput", () => {
  it("omits an untouched string field that has no default, like an API caller leaving the key out", () => {
    const input = buildExecuteInput({ dataset: "", day: "" }, fields);
    expect(input).toEqual({});
    expect("dataset" in input).toBe(false);
  });

  it("keeps a string the user actually typed", () => {
    expect(buildExecuteInput({ dataset: "ala_test" }, fields)).toEqual({ dataset: "ala_test" });
  });

  it("keeps an empty string when the field declares a default, so clearing a defaulted field is explicit", () => {
    expect(buildExecuteInput({ version: "", env: "" }, fields)).toEqual({ version: "", env: "" });
  });

  it("keeps boolean false: an unchecked box is what the user sees", () => {
    expect(buildExecuteInput({ skip: false }, fields)).toEqual({ skip: false });
  });

  it("omits an empty number field with no default instead of sending 0", () => {
    expect(buildExecuteInput({ limit: "" }, fields)).toEqual({});
  });

  it("converts a typed number to a JSON number", () => {
    expect(buildExecuteInput({ limit: "42", count: "7" }, fields)).toEqual({ limit: 42, count: 7 });
  });

  it("drops the secret sentinel so the server applies the schema default", () => {
    expect(buildExecuteInput({ token: SECRET_SENTINEL }, fields)).toEqual({});
  });

  it("forwards the redaction sentinel unchanged for re-run replay", () => {
    expect(buildExecuteInput({ token: REDACTED_SENTINEL }, fields)).toEqual({
      token: REDACTED_SENTINEL,
    });
  });

  it("omits an unselected connection field with no default", () => {
    expect(buildExecuteInput({ clickhouse: "" }, fields)).toEqual({});
  });
});
