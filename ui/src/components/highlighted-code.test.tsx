import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { HighlightedCode } from "./highlighted-code";

describe("HighlightedCode", () => {
  it("renders classed spans and keeps the text unchanged", () => {
    const code = '{\n  "a": [1, true, null]\n}';
    const { container } = render(
      <pre data-testid="p">
        <HighlightedCode code={code} language="json" />
      </pre>,
    );
    const pre = container.querySelector("pre")!;
    expect(pre.textContent).toBe(code);
    expect(pre.querySelector("span.tok-propertyName")?.textContent).toBe('"a"');
    expect(pre.querySelector("span.tok-number")?.textContent).toBe("1");
    expect(pre.querySelector("span.tok-bool")?.textContent).toBe("true");
  });
});
