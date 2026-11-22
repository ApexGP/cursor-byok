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
  assert.doesNotMatch(notes.markdown, /Thanks to contributors/);
});

test("credits pull request authors on their commits and in a thanks section", () => {
  const notes = buildReleaseNotes({
    version: "1.0.9",
    repository: "ApexGP/cursor-byok",
    previousTag: "v1.0.8",
    commits: [
      {
        hash: "980c547",
        fullHash: "980c547aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        subject: "fix(cursor): accept binary BidiAppend payloads",
      },
      {
        hash: "6ea72fc",
        fullHash: "6ea72fcbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        subject: "fix(process): treat a gone Cursor process as a successful stop",
      },
    ],
    pullRequests: [
      {
        number: 3,
        title: "fix(cursor): accept binary BidiAppend payloads",
        author: { login: "xiaochaodada" },
        url: "https://github.com/ApexGP/cursor-byok/pull/3",
        commits: ["980c547aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"],
      },
    ],
  });

  assert.match(
    notes.markdown,
    /- fix\(cursor\): accept binary BidiAppend payloads \(`980c547`\) — thanks @xiaochaodada/,
  );
  assert.doesNotMatch(notes.markdown, /6ea72fc`\) — thanks/);
  assert.match(notes.markdown, /### Thanks to contributors/);
  assert.match(
    notes.markdown,
    /- @xiaochaodada for \[#3\]\(https:\/\/github\.com\/ApexGP\/cursor-byok\/pull\/3\) — fix\(cursor\): accept binary BidiAppend payloads/,
  );
  assert.equal(notes.contributors.length, 1);
});

test("groups multiple pull requests from the same contributor", () => {
  const notes = buildReleaseNotes({
    version: "1.0.9",
    repository: "ApexGP/cursor-byok",
    previousTag: "v1.0.8",
    commits: [],
    pullRequests: [
      {
        number: 3,
        title: "First fix",
        author: "alice",
        url: "https://github.com/ApexGP/cursor-byok/pull/3",
        commits: [],
      },
      {
        number: 5,
        title: "Second fix",
        author: { login: "alice" },
        url: "not-a-github-url",
        commits: [],
      },
      {
        number: 7,
        title: "Invalid author",
        author: { login: "not a login" },
        url: "https://github.com/ApexGP/cursor-byok/pull/7",
        commits: [],
      },
    ],
  });

  assert.match(
    notes.markdown,
    /- @alice for \[#3\]\(https:\/\/github\.com\/ApexGP\/cursor-byok\/pull\/3\), \[#5\]\(https:\/\/github\.com\/ApexGP\/cursor-byok\/pull\/5\)/,
  );
  assert.doesNotMatch(notes.markdown, /not a login/);
  assert.equal(notes.contributors.length, 1);
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
