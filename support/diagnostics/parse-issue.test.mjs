import assert from "node:assert/strict";
import { test } from "node:test";
import { extractDiagnostic } from "./parse-issue.mjs";

const body = `## 诊断数据
<!-- CURSOR_BYOK_DIAGNOSTIC_START -->
\`\`\`json
{"schemaVersion":1,"errorCode":"CB-SYNC-001","logs":{"tail":[]}}
\`\`\`
<!-- CURSOR_BYOK_DIAGNOSTIC_END -->`;

test("extracts and parses the diagnostic json block", () => {
  assert.deepEqual(extractDiagnostic(body), {
    schemaVersion: 1,
    errorCode: "CB-SYNC-001",
    logs: { tail: [] },
  });
});

test("rejects a body without markers", () => {
  assert.throws(() => extractDiagnostic("{}"), /markers not found/);
});

test("rejects malformed json", () => {
  const malformed = body.replace('{"schemaVersion":1', '{"schemaVersion":');
  assert.throws(() => extractDiagnostic(malformed), /invalid diagnostic block/);
});
