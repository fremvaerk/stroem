import { SECRET_SENTINEL } from "@/components/task/constants";
import { isJsonFieldState, parseJsonText } from "./json-field";
import type { InputField } from "./types";

/** A field the form cannot submit as it stands. */
export class ExecuteFormError extends Error {
  field: string;
  constructor(field: string, message: string) {
    super(`${field}: ${message}`);
    this.field = field;
  }
}

export interface ExecutePayload {
  input: Record<string, unknown>;
  /** Fields replayed from the re-run source (spec D12). */
  replayFields: string[];
}

/**
 * Turn the execute form's values into the payload for
 * `POST /tasks/{name}/execute`.
 *
 * The rule that matters: a field the user left EMPTY and that declares no
 * `default` is OMITTED, not sent as `""`. That makes a UI-submitted job look
 * like an API/MCP call that leaves the key out, so `{{ input.x | default(...) }}`
 * fires the same way from both entry points (Tera's `default` only applies to
 * an absent variable, never to an empty string). An empty value on a field
 * WITH a default is kept: the user cleared a pre-filled value on purpose.
 *
 * A `json` field's wire value follows from its MODE alone:
 * default → omitted, replay → named in `replayFields`, value → parsed text.
 */
export function buildExecutePayload(
  values: Record<string, unknown>,
  fields: Record<string, InputField>,
): ExecutePayload {
  const input: Record<string, unknown> = {};
  const replayFields: string[] = [];
  for (const [key, val] of Object.entries(values)) {
    const field = fields[key];
    if (isJsonFieldState(val)) {
      if (val.mode === "default") continue;
      if (val.mode === "replay") {
        replayFields.push(key);
        continue;
      }
      if (val.text.trim() === "") {
        if (field?.required && field.default === undefined) {
          throw new ExecuteFormError(key, "a value is required");
        }
        continue;
      }
      const parsed = parseJsonText(val.text);
      if (!parsed.ok) throw new ExecuteFormError(key, parsed.error);
      input[key] = parsed.value;
      continue;
    }
    // "********" means "secret unchanged from its stored default": drop it so
    // the server applies the schema default. The redaction marker "••••••" is a
    // different string and passes through for re-run replay.
    if (field?.secret && val === SECRET_SENTINEL) continue;
    if (val === "" && field?.default === undefined) continue;
    input[key] = field?.type === "number" ? Number(val) : val;
  }
  return { input, replayFields };
}

/** The `input` part of {@link buildExecutePayload}. */
export function buildExecuteInput(
  values: Record<string, unknown>,
  fields: Record<string, InputField>,
): Record<string, unknown> {
  return buildExecutePayload(values, fields).input;
}
