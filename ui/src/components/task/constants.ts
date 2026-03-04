export const SECRET_SENTINEL = "********";
// Must match PRIMITIVE_TYPES in crates/stroem-common/src/template.rs
export const PRIMITIVE_TYPES = new Set([
  "string",
  "text",
  "integer",
  "number",
  "boolean",
  "date",
  "datetime",
]);
