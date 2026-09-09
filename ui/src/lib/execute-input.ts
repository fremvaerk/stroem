import { SECRET_SENTINEL } from "@/components/task/constants";
import type { InputField } from "./types";

/**
 * Turn the execute form's values into the `input` payload for
 * `POST /tasks/{name}/execute`.
 *
 * The rule that matters: a field the user left EMPTY and that declares no
 * `default` is OMITTED, not sent as `""`. That makes a UI-submitted job look
 * like an API/MCP call that leaves the key out, so `{{ input.x | default(...) }}`
 * fires the same way from both entry points (Tera's `default` only applies to
 * an absent variable, never to an empty string). An empty value on a field
 * WITH a default is kept: the user cleared a pre-filled value on purpose.
 */
export function buildExecuteInput(
  values: Record<string, unknown>,
  fields: Record<string, InputField>,
): Record<string, unknown> {
  const input: Record<string, unknown> = {};
  for (const [key, val] of Object.entries(values)) {
    const field = fields[key];
    // "********" means "secret unchanged from its stored default": drop it so
    // the server applies the schema default. The redaction marker "••••••" is a
    // different string and passes through for re-run replay.
    if (field?.secret && val === SECRET_SENTINEL) continue;
    if (val === "" && field?.default === undefined) continue;
    input[key] = field?.type === "number" ? Number(val) : val;
  }
  return input;
}
