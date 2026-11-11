import assert from "node:assert/strict";
import { test } from "node:test";
import {
  expiresAt,
  objectKey,
  validateReportId,
  validateSha256,
  validateZip,
} from "../src/validate.js";

test("accepts only the upload report-id shape", () => {
  assert.equal(validateReportId("r-20261111-0123456789abcdef0123456789abcdef"), true);
  assert.equal(validateReportId("r-20261111-abc"), false);
  assert.equal(validateReportId("r-20261111-0123456789ABCDEF0123456789abcdef"), false);
});

test("accepts only lowercase sha256", () => {
  assert.equal(validateSha256("a".repeat(64)), true);
  assert.equal(validateSha256("A".repeat(64)), false);
  assert.equal(validateSha256("a".repeat(63)), false);
});

test("rejects non-zip bodies", () => {
  assert.equal(validateZip(new TextEncoder().encode("not a zip")), false);
  assert.equal(validateZip(new Uint8Array([0x50, 0x4b, 0x03, 0x04])), true);
});

test("object keys stay under the reports prefix", () => {
  assert.equal(
    objectKey("r-20261111-0123456789abcdef0123456789abcdef"),
    "reports/r-20261111-0123456789abcdef0123456789abcdef.zip",
  );
  assert.throws(() => objectKey("../escape"), /invalid report id/);
});

test("expiry uses the configured retention window", () => {
  const now = new Date("2026-11-11T00:00:00Z");
  assert.equal(expiresAt(now, 30).toISOString(), "2026-12-11T00:00:00.000Z");
});
