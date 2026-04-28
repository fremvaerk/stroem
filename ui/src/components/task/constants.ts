export const SECRET_SENTINEL = "********";
// Server-side redaction marker used in API responses (and on the wire when the UI
// asks the server to "replay this from the source job's raw_input"). Must match
// the Rust REDACTED constant in crates/stroem-server/src/web/api/jobs.rs.
export const REDACTED_SENTINEL = "••••••";
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
