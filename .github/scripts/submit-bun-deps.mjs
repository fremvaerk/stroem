#!/usr/bin/env node
// Submit a bun.lock to GitHub's Dependency Submission API.
//
// bun.lock is JSONC (JSON with comments and trailing commas). Neither
// GitHub's dependency graph nor Syft parses it yet (anchore/syft#4862,
// in progress as of May 2026), so we submit ourselves to keep Dependabot
// alerts in sync with the lockfile after each push to main.
//
// Format reference:
//   {
//     "lockfileVersion": 1,
//     "workspaces": { "": { "name": "...", "dependencies": {...}, "devDependencies": {...} } },
//     "packages": {
//       "<key>": ["<name>@<version>", "<registry>", { ...metadata }, "<integrity>"]
//     }
//   }

import fs from "node:fs";
import process from "node:process";

const lockPath = process.argv[2];
if (!lockPath) {
  console.error("Usage: submit-bun-deps.mjs <path/to/bun.lock>");
  process.exit(1);
}

const raw = fs.readFileSync(lockPath, "utf8");
const lock = JSON.parse(stripJsonc(raw));

// Collect names declared as direct deps across all workspaces. bun.lock
// `packages` keys can be nested (e.g. "react/scheduler") for transitive
// resolutions, so we match on the trailing segment.
const directNames = new Set();
const devNames = new Set();
for (const ws of Object.values(lock.workspaces ?? {})) {
  for (const name of Object.keys(ws.dependencies ?? {})) directNames.add(name);
  for (const name of Object.keys(ws.peerDependencies ?? {})) directNames.add(name);
  for (const name of Object.keys(ws.optionalDependencies ?? {})) directNames.add(name);
  for (const name of Object.keys(ws.devDependencies ?? {})) {
    directNames.add(name);
    devNames.add(name);
  }
}

const resolved = {};
for (const [key, entry] of Object.entries(lock.packages ?? {})) {
  if (!Array.isArray(entry) || typeof entry[0] !== "string") continue;
  const spec = entry[0];
  const at = spec.lastIndexOf("@");
  if (at <= 0) continue;
  const name = spec.slice(0, at);
  const version = spec.slice(at + 1);
  if (version.startsWith("workspace:")) continue;

  // For a nested key like "react-router/cookie", the leaf name is the
  // package's own name (`name`); the key prefix is just the resolution path.
  const isDirect = directNames.has(name);
  const isDev = devNames.has(name);

  resolved[key] = {
    package_url: `pkg:npm/${name}@${encodeURIComponent(version)}`,
    relationship: isDirect ? "direct" : "indirect",
    scope: isDev ? "development" : "runtime",
  };
}

const repo = required("GITHUB_REPOSITORY");
const sha = required("GITHUB_SHA");
const ref = required("GITHUB_REF");
const runId = required("GITHUB_RUN_ID");
const token = required("GITHUB_TOKEN");

const snapshot = {
  version: 0,
  sha,
  ref,
  job: { id: runId, correlator: `bun-${lockPath}` },
  detector: {
    name: "stroem-bun-submission",
    version: "0.1.0",
    url: "https://github.com/fremvaerk/stroem",
  },
  scanned: new Date().toISOString(),
  manifests: {
    [lockPath]: {
      name: lockPath,
      file: { source_location: lockPath },
      resolved,
    },
  },
};

const url = `https://api.github.com/repos/${repo}/dependency-graph/snapshots`;
const res = await fetch(url, {
  method: "POST",
  headers: {
    Accept: "application/vnd.github+json",
    Authorization: `Bearer ${token}`,
    "X-GitHub-Api-Version": "2022-11-28",
    "Content-Type": "application/json",
  },
  body: JSON.stringify(snapshot),
});

if (!res.ok) {
  console.error(`Snapshot submission failed: ${res.status} ${res.statusText}`);
  console.error(await res.text());
  process.exit(1);
}
const body = await res.json().catch(() => ({}));
console.log(
  `Submitted ${Object.keys(resolved).length} packages from ${lockPath} (id=${body.id ?? "?"})`,
);

function stripJsonc(text) {
  // bun.lock is technically JSONC but only ever uses trailing commas — never
  // // or /* */ comments. A naive comment-stripper is unsafe because base64
  // sha512 integrity hashes routinely contain "//" (e.g. "sha512-...//...==").
  // Stripping only trailing commas keeps the parser correct without false matches.
  return text.replace(/,(\s*[}\]])/g, "$1");
}

function required(name) {
  const v = process.env[name];
  if (!v) {
    console.error(`Missing required env var: ${name}`);
    process.exit(1);
  }
  return v;
}
