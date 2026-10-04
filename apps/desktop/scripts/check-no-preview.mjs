#!/usr/bin/env node
// SPDX-License-Identifier: MIT
//
// Leak guard for the dev-only browser preview (src/preview/README.md): a
// release build must never ship the preview's mock IPC or fixture data.
// Run after `npm run build`, against the built `dist` directory.
//
// Fails (exit 1) if any built file's *contents* contain the preview marker
// (src/preview/mockTauri.ts's MACALLA_BROWSER_PREVIEW_MARKER_7f3c), or if
// any built file *name* contains "preview" -- catching both "the code got
// bundled in" and "a preview-named chunk exists at all". Node only, no
// dependencies, so it runs in CI with nothing but `node`.

import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative } from "node:path";

const DIST_DIR = "dist";
const MARKER = "MACALLA_BROWSER_PREVIEW_MARKER_7f3c";

function walk(dir) {
  const out = [];
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    const stat = statSync(full);
    if (stat.isDirectory()) {
      out.push(...walk(full));
    } else {
      out.push(full);
    }
  }
  return out;
}

let files;
try {
  files = walk(DIST_DIR);
} catch (err) {
  console.error(`check-no-preview: could not read "${DIST_DIR}" (run "npm run build" first): ${err.message}`);
  process.exit(1);
}

const hits = [];

for (const file of files) {
  const rel = relative(".", file);
  if (rel.toLowerCase().includes("preview")) {
    hits.push(`file name contains "preview": ${rel}`);
    continue;
  }
  let contents;
  try {
    contents = readFileSync(file, "utf8");
  } catch {
    // A binary asset (font, image, ...): never going to contain the marker
    // as text, and its name already passed the check above.
    continue;
  }
  if (contents.includes(MARKER)) {
    const line = contents.slice(0, contents.indexOf(MARKER)).split("\n").length;
    hits.push(`marker "${MARKER}" found in ${rel} (line ${line})`);
  }
}

if (hits.length > 0) {
  console.error("check-no-preview: the browser preview leaked into the release build:");
  for (const hit of hits) {
    console.error(`  - ${hit}`);
  }
  process.exit(1);
}

console.log(`check-no-preview: clean (${files.length} files checked in "${DIST_DIR}")`);
