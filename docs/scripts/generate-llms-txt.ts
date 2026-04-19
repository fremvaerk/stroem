/**
 * Generates LLM-directed documentation outputs from the Starlight sources.
 *
 * Produces three outputs in `docs/public/`:
 *   - `llms.txt` — lightweight index (llmstxt.org spec) with links to per-section files
 *   - `llms-full.txt` — single-file reference with every section inlined (legacy consumers)
 *   - `llms/<slug>.md` — one file per section, fetched on demand by LLM agents
 *
 * Run: bun run scripts/generate-llms-txt.ts
 * Runs automatically before `astro build` via the build script.
 */

import { mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const docsRoot = join(__dirname, "..", "src", "content", "docs");
const publicDir = join(__dirname, "..", "public");
const sectionsDir = join(publicDir, "llms");
const indexPath = join(publicDir, "llms.txt");
const fullPath = join(publicDir, "llms-full.txt");

const BASE_URL = "https://fremvaerk.github.io/stroem";

// Inlined into both llms.txt (as "Validation rules") and llms-full.txt (as header).
const VALIDATION_RULES = `## Quick Reference — Validation Rules

When generating Strøm workflow YAML, these constraints are enforced at parse time:

**Action types:**
- \`type: script\` — requires \`script\` or \`source\`. Cannot have \`image\`. Supports \`language\` field: \`shell\` (default), \`python\`, \`javascript\`, \`typescript\`, \`go\`. Optional \`dependencies\` (package list) and \`interpreter\` (override auto-detected binary).
- \`type: docker\` — requires \`image\`. Runs container as-is (no workspace mount).
- \`type: pod\` — requires \`image\`. Runs as Kubernetes pod (no workspace mount).
- \`type: script\` + \`runner: docker\` — scripts inside a Docker container with workspace at \`/workspace\`.
- \`type: script\` + \`runner: pod\` — scripts inside a Kubernetes pod with workspace.
- \`type: task\` — requires \`task\` field. Cannot have \`cmd\`, \`script\`, \`image\`, \`runner\`, or \`language\`.
- \`type: agent\` — requires \`provider\` and \`prompt\`. Calls an LLM (19 providers: anthropic, azure, cohere, deepseek, galadriel, gemini, groq, huggingface, hyperbolic, llamafile, mira, mistral, moonshot, ollama, openai, openrouter, perplexity, together, xai). Optional \`system_prompt\`, \`output\` (structured JSON schema), \`model\`, \`temperature\`, \`max_tokens\`.
- \`type: approval\` — optional \`message\` (Tera template). Pauses job for human approval, reuses Phase 5d infrastructure.
- \`type: script\` + \`image\` is **rejected** — use \`type: docker\` or \`runner: docker\` instead.

**Task actions:**
- \`task\` field must reference an existing task in the same workspace.
- Self-referencing tasks (direct or via hooks) are rejected.
- Maximum nesting depth: 10 levels.

**Flow steps:**
- \`action\` must reference an existing action.
- \`depends_on\` must reference existing steps within the same flow.
- No cycles allowed in the dependency graph.
- Step names must not contain \`[\` or \`]\` (reserved for for_each instances).
- \`for_each\`: Tera template string (renders to JSON array) or literal array. Max 10,000 items.
- \`sequential: true\`: run loop instances one at a time (default: parallel).
- Inside loop instances: \`each.item\` = current element, \`each.index\` = zero-based index.

**Triggers:**
- \`type: scheduler\` — \`cron\` must be a valid 5-field or 6-field cron expression.
- \`type: webhook\` — \`name\` must match \`^[a-zA-Z0-9_-]+$\` (URL-safe).
- \`mode\` must be \`"sync"\` or \`"async"\` (default: \`"async"\`).
- \`timeout_secs\` must be 1–300 (default: 30). Only meaningful in sync mode.
- \`type: event_source\` — long-running queue consumer. Requires \`task\` (consumer task) and \`target_task\` (target for emitted jobs). Consumer's execution defined in referenced task. Emitted JSON on stdout creates jobs for target task. \`env:\` provides environment overrides. \`restart_policy\`: \`always\` (default), \`on_failure\`, \`never\`. \`backoff_secs\` for exponential backoff. \`max_in_flight\` for backpressure.
- \`force_refresh: true\` (scheduler, webhook) — reload workspace from source before creating the job.

**Hooks:**
- \`on_success\` / \`on_error\` hook action references must exist.
- Hook actions can be \`type: task\` (creates a child job).

**Templates:**
- Step names with hyphens (e.g., \`say-hello\`) become underscores in templates: \`{{ say_hello.output.* }}\`.
- Template context: \`input.*\`, \`<step_name>.output.*\`, \`secret.*\`, \`state.*\` (task state), \`global_state.*\` (workspace state).

**Pod manifest overrides:**
- \`manifest\` field is only valid on \`type: pod\` and \`type: script\` + \`runner: pod\`.
- Must be a JSON/YAML object (not array or scalar).

**Connections:**
- \`connection_types\` define property schemas (\`string\`, \`integer\`, \`number\`, \`boolean\`).
- \`connections\` are named instances referencing a type. Required fields must be present.
- Task input \`type\` can reference a connection type — the name is resolved to the full object at job creation.
- Connection values support Tera templates (e.g., \`{{ 'ref+...' | vals }}\`).`;

// Section metadata drives all three outputs.
// `group` controls the index heading; `desc` appears after the link so an LLM can
// decide what to fetch without reading the file.
type Section = {
  file: string;
  title: string;
  slug: string;
  desc: string;
  group: "Guides" | "Reference" | "Examples";
};

const sections: Section[] = [
  { file: "guides/workflow-basics.md", title: "Workflow Basics", slug: "workflow-basics",
    desc: "Fundamentals: workspaces, tasks, actions, flow steps", group: "Guides" },
  { file: "guides/action-types.md", title: "Action Types", slug: "action-types",
    desc: "script, docker, pod, task, agent, approval — fields and constraints", group: "Guides" },
  { file: "guides/input-and-output.md", title: "Input & Output", slug: "input-and-output",
    desc: "Task input schemas, step outputs, and OUTPUT: stdout protocol", group: "Guides" },
  { file: "guides/templating.md", title: "Templating", slug: "templating",
    desc: "Tera expressions, template context, secrets, state variables", group: "Guides" },
  { file: "guides/conditionals.md", title: "Conditional Flow Steps", slug: "conditionals",
    desc: "when expressions for conditional step execution", group: "Guides" },
  { file: "guides/loops.md", title: "For-Each Loops", slug: "loops",
    desc: "for_each with parallel and sequential execution modes", group: "Guides" },
  { file: "guides/retry.md", title: "Retry Mechanisms", slug: "retry",
    desc: "Step-level and task-level retry with backoff and jitter", group: "Guides" },
  { file: "guides/triggers.md", title: "Triggers", slug: "triggers",
    desc: "Scheduler (cron) and webhook triggers, including force_refresh", group: "Guides" },
  { file: "guides/event-sources.md", title: "Event Source Triggers", slug: "event-sources",
    desc: "Long-running queue consumer triggers with OUTPUT: protocol", group: "Guides" },
  { file: "guides/hooks.md", title: "Hooks", slug: "hooks",
    desc: "on_success, on_error, on_cancel, on_suspended lifecycle hooks", group: "Guides" },
  { file: "guides/connections.md", title: "Connections", slug: "connections",
    desc: "Typed external system configs (databases, APIs, credentials)", group: "Guides" },
  { file: "guides/secrets.md", title: "Secrets & Encryption", slug: "secrets",
    desc: "vals-based secret references (vault, AWS SSM, env vars)", group: "Guides" },
  { file: "guides/agent-actions.md", title: "Agent Actions", slug: "agent-actions",
    desc: "LLM calls as workflow steps (19 providers, structured output, tools)", group: "Guides" },
  { file: "guides/task-state.md", title: "Task State Snapshots", slug: "task-state",
    desc: "Cross-run state persistence: STATE:, GLOBAL_STATE:, /state mount", group: "Guides" },
  { file: "guides/mcp.md", title: "MCP Integration", slug: "mcp",
    desc: "Model Context Protocol server for exposing Strøm to AI agents", group: "Guides" },
  { file: "reference/workflow-yaml.md", title: "Workflow YAML Reference", slug: "workflow-yaml",
    desc: "Complete YAML schema reference for all fields", group: "Reference" },
  { file: "examples/hello-world.md", title: "Hello World", slug: "hello-world",
    desc: "Minimal single-step workflow", group: "Examples" },
  { file: "examples/deploy-pipeline.md", title: "Deploy Pipeline", slug: "deploy-pipeline",
    desc: "Multi-stage deploy with approval gate and rollback", group: "Examples" },
  { file: "examples/ci-pipeline.md", title: "CI Pipeline", slug: "ci-pipeline",
    desc: "Test → build → publish with matrix and conditionals", group: "Examples" },
];

function stripFrontmatter(content: string): string {
  const match = content.match(/^---\n[\s\S]*?\n---\n/);
  return match ? content.slice(match[0].length) : content;
}

function convertStarlightSyntax(content: string): string {
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

function buildIndex(): string {
  const groups: Record<Section["group"], Section[]> = {
    Guides: [],
    Reference: [],
    Examples: [],
  };
  for (const s of sections) groups[s.group].push(s);

  const groupBlock = (name: Section["group"]) => {
    const items = groups[name]
      .map((s) => `- [${s.title}](${BASE_URL}/llms/${s.slug}.md): ${s.desc}`)
      .join("\n");
    return `## ${name}\n\n${items}`;
  };

  return `# Strøm — LLM Reference

> Strøm is a workflow/task orchestration platform. Workflows are defined as YAML
> files in a \`.workflows/\` directory. This index lists documentation sections
> available at focused URLs so LLM agents can fetch only what they need.
>
> Site: ${BASE_URL}

${VALIDATION_RULES}

${groupBlock("Guides")}

${groupBlock("Reference")}

${groupBlock("Examples")}

## Single-file reference

If your client cannot fetch individual sections, fetch [llms-full.txt](${BASE_URL}/llms-full.txt)
for everything inlined (~200 KB).
`;
}

function buildFull(): string {
  const parts: string[] = [
    `# Strøm — Workflow YAML Reference

> Strøm is a workflow/task orchestration platform. Workflows are defined as YAML
> files in a \`.workflows/\` directory. This document contains everything needed
> to author valid workflow YAML: actions, tasks, triggers, hooks, templating,
> input/output, and secrets.
>
> Site: ${BASE_URL}

${VALIDATION_RULES}`,
  ];
  for (const s of sections) {
    const content = processFile(join(docsRoot, s.file));
    parts.push(`## ${s.title}\n\n${content}`);
  }
  return parts.join("\n\n---\n\n") + "\n";
}

// Ensure clean output directory for per-section files.
rmSync(sectionsDir, { recursive: true, force: true });
mkdirSync(sectionsDir, { recursive: true });

// 1. Per-section files.
for (const s of sections) {
  const content = processFile(join(docsRoot, s.file));
  const sectionBody = `# ${s.title}\n\n${content}\n`;
  writeFileSync(join(sectionsDir, `${s.slug}.md`), sectionBody, "utf-8");
}

// 2. llms-full.txt (legacy single-file consumers).
const full = buildFull();
writeFileSync(fullPath, full, "utf-8");

// 3. llms.txt (new lightweight index).
const index = buildIndex();
writeFileSync(indexPath, index, "utf-8");

const kb = (p: string) => (Buffer.byteLength(readFileSync(p), "utf-8") / 1024).toFixed(1);
console.log(
  `Generated:\n  ${indexPath} (${kb(indexPath)} KB)\n  ${fullPath} (${kb(fullPath)} KB)\n  ${sectionsDir}/ (${sections.length} files)`,
);
