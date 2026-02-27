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
