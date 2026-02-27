/**
 * Generates llms.txt from the documentation source files.
 *
 * Run: bun run scripts/generate-llms-txt.ts
 * Runs automatically before `astro build` via the build script.
 *
 * Output: docs/public/llms.txt (served at /llms.txt by Astro)
 */

import { readFileSync, writeFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const docsRoot = join(__dirname, "..", "src", "content", "docs");
const outPath = join(__dirname, "..", "public", "llms.txt");

const HEADER = `# Strøm — Workflow YAML Reference

> Strøm is a workflow/task orchestration platform. Workflows are defined as YAML
> files in a \`.workflows/\` directory. This document contains everything needed
> to author valid workflow YAML: actions, tasks, triggers, hooks, templating,
> input/output, and secrets.
>
> Site: https://fremvaerk.github.io/stroem

## Quick Reference — Validation Rules

When generating Strøm workflow YAML, these constraints are enforced at parse time:

**Action types:**
- \`type: shell\` — requires \`cmd\` or \`script\`. Cannot have \`image\`.
- \`type: docker\` — requires \`image\`. Runs container as-is (no workspace mount).
- \`type: pod\` — requires \`image\`. Runs as Kubernetes pod (no workspace mount).
- \`type: shell\` + \`runner: docker\` — shell commands inside a Docker container with workspace at \`/workspace\`.
- \`type: shell\` + \`runner: pod\` — shell commands inside a Kubernetes pod with workspace.
- \`type: task\` — requires \`task\` field. Cannot have \`cmd\`, \`script\`, \`image\`, or \`runner\`.
- \`type: shell\` + \`image\` is **rejected** — use \`type: docker\` or \`runner: docker\` instead.

**Task actions:**
- \`task\` field must reference an existing task in the same workspace.
- Self-referencing tasks (direct or via hooks) are rejected.
- Maximum nesting depth: 10 levels.

**Flow steps:**
- \`action\` must reference an existing action.
- \`depends_on\` must reference existing steps within the same flow.
- No cycles allowed in the dependency graph.

**Triggers:**
- \`type: scheduler\` — \`cron\` must be a valid 5-field or 6-field cron expression.
- \`type: webhook\` — \`name\` must match \`^[a-zA-Z0-9_-]+$\` (URL-safe).
- \`mode\` must be \`"sync"\` or \`"async"\` (default: \`"async"\`).
- \`timeout_secs\` must be 1–300 (default: 30). Only meaningful in sync mode.

**Hooks:**
- \`on_success\` / \`on_error\` hook action references must exist.
- Hook actions can be \`type: task\` (creates a child job).

**Templates:**
- Step names with hyphens (e.g., \`say-hello\`) become underscores in templates: \`{{ say_hello.output.* }}\`.
- Template context: \`input.*\`, \`<step_name>.output.*\`, \`secret.*\`.

**Pod manifest overrides:**
- \`manifest\` field is only valid on \`type: pod\` and \`type: shell\` + \`runner: pod\`.
- Must be a JSON/YAML object (not array or scalar).

`;

// Files to include, in order. Paths relative to docs/src/content/docs/.
const sections: { file: string; title: string }[] = [
  { file: "guides/workflow-basics.md", title: "Workflow Basics" },
  { file: "guides/action-types.md", title: "Action Types" },
  { file: "guides/input-and-output.md", title: "Input & Output" },
  { file: "guides/templating.md", title: "Templating" },
  { file: "guides/triggers.md", title: "Triggers" },
  { file: "guides/hooks.md", title: "Hooks" },
  { file: "guides/secrets.md", title: "Secrets & Encryption" },
  { file: "examples/hello-world.md", title: "Example: Hello World" },
  { file: "examples/deploy-pipeline.md", title: "Example: Deploy Pipeline" },
  { file: "examples/ci-pipeline.md", title: "Example: CI Pipeline" },
];

function stripFrontmatter(content: string): string {
  const match = content.match(/^---\n[\s\S]*?\n---\n/);
  return match ? content.slice(match[0].length) : content;
}

function convertStarlightSyntax(content: string): string {
  // Convert :::note / :::caution / :::tip blocks to plain text
  return content.replace(
    /^:::(note|caution|tip|danger|warning)\n([\s\S]*?)^:::\s*$/gm,
    (_match, type: string, body: string) => {
      const label = type.toUpperCase();
      const trimmed = body.trim();
      return `**${label}:** ${trimmed}`;
    }
  );
}

function processFile(filePath: string): string {
  const raw = readFileSync(filePath, "utf-8");
  let content = stripFrontmatter(raw);
  content = convertStarlightSyntax(content);
  return content.trim();
}

// Build output
const parts: string[] = [HEADER];

for (const section of sections) {
  const filePath = join(docsRoot, section.file);
  const content = processFile(filePath);
  parts.push(`## ${section.title}\n\n${content}`);
}

const output = parts.join("\n\n---\n\n") + "\n";

writeFileSync(outPath, output, "utf-8");

const lines = output.split("\n").length;
const bytes = Buffer.byteLength(output, "utf-8");
console.log(`Generated llms.txt: ${lines} lines, ${(bytes / 1024).toFixed(1)} KB → ${outPath}`);
