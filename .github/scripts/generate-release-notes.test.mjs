import assert from "node:assert/strict";
import test from "node:test";
import { buildReleaseNotes } from "./generate-release-notes.mjs";

test("builds a release note from commits since the previous tag", () => {
  const notes = buildReleaseNotes({
    version: "v1.0.3",
    repository: "ApexGP/cursor-byok",
    previousTag: "v1.0.2",
    commits: [
      {
        hash: "abc1234",
        subject: "Remove external API and plugin page",
        details: ["Delete /byok/v1 routes", "Remove plugin management UI"],
      },
      { hash: "def5678", subject: "Generate release notes in CI" },
    ],
  });

  assert.match(notes.markdown, /## Cursor BYOK v1\.0\.3/);
  assert.match(notes.markdown, /- Remove external API and plugin page \(`abc1234`\)/);
  assert.match(notes.markdown, /  - Delete \/byok\/v1 routes/);
  assert.match(notes.markdown, /  - Remove plugin management UI/);
  assert.match(notes.markdown, /- Generate release notes in CI \(`def5678`\)/);
  assert.equal(notes.range, "v1.0.2...v1.0.3");
  assert.match(notes.markdown, /https:\/\/github\.com\/ApexGP\/cursor-byok\/compare\/v1\.0\.2\.\.\.v1\.0\.3/);
});

test("marks beta versions and ignores empty commit subjects", () => {
  const notes = buildReleaseNotes({
    version: "1.0.3-beta.1",
    repository: "ApexGP/cursor-byok",
    previousTag: "",
    commits: [
      { hash: "ccc3333", subject: "" },
      { hash: "ddd4444", subject: "Fix startup race" },
    ],
  });

  assert.match(notes.markdown, /> \*\*Beta release\.\*\* This is a test version\./);
  assert.match(notes.markdown, /- Fix startup race \(`ddd4444`\)/);
  assert.doesNotMatch(notes.markdown, /-  \(`ccc3333`\)/);
});

test("filters workflow trigger commits and handles empty history", () => {
  const notes = buildReleaseNotes({
    version: "1.0.3",
    repository: "ApexGP/cursor-byok",
    previousTag: "",
    commits: [{ hash: "aaa1111", subject: "ci: trigger workflows" }],
  });

  assert.match(notes.markdown, /- Maintenance release\./);
  assert.doesNotMatch(notes.markdown, /trigger workflows/);
  assert.doesNotMatch(notes.markdown, /Compare changes/);
});

test("rejects invalid version and repository", () => {
  assert.throws(
    () => buildReleaseNotes({ version: "latest", repository: "ApexGP/cursor-byok", commits: [] }),
    /semantic version/,
  );
  assert.throws(
    () => buildReleaseNotes({ version: "1.0.3", repository: "", commits: [] }),
    /GitHub repository/,
  );
});
