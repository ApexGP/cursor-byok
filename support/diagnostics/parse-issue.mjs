#!/usr/bin/env node
import { readFileSync } from "node:fs";

const START = "<!-- CURSOR_BYOK_DIAGNOSTIC_START -->";
const END = "<!-- CURSOR_BYOK_DIAGNOSTIC_END -->";

export function extractDiagnostic(body) {
  const start = body.indexOf(START);
  const end = body.indexOf(END, start + START.length);
  if (start < 0 || end < 0) throw new Error("invalid diagnostic block: markers not found");
  const block = body.slice(start + START.length, end).trim();
  const fenced = block.match(/^```json\s*([\s\S]*?)\s*```$/i);
  const json = fenced ? fenced[1] : block;
  try {
    return JSON.parse(json);
  } catch (error) {
    throw new Error(`invalid diagnostic block: ${error.message}`);
  }
}

function main() {
  const args = process.argv.slice(2);
  const fieldIndex = args.indexOf("--field");
  const field = fieldIndex >= 0 ? args[fieldIndex + 1] : null;
  if (fieldIndex >= 0 && !field) throw new Error("--field requires a name");
  const body = readFileSync(0, "utf8");
  const diagnostic = extractDiagnostic(body);
  if (field) {
    if (!(field in diagnostic)) throw new Error(`missing field: ${field}`);
    process.stdout.write(`${JSON.stringify(diagnostic[field])}\n`);
  } else {
    process.stdout.write(`${JSON.stringify(diagnostic)}\n`);
  }
}

if (import.meta.url === `file://${process.argv[1]}`) {
  try {
    main();
  } catch (error) {
    console.error(error.message);
    process.exit(2);
  }
}
