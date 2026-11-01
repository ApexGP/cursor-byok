import { execFileSync } from "node:child_process";
import { writeFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";

const VERSION_PATTERN = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/;
const IGNORED_SUBJECTS = [/^ci:\s*trigger workflows$/i];

function readOptions(args) {
  const options = new Map();
  for (let index = 0; index < args.length; index += 2) {
    const name = args[index];
    const value = args[index + 1];
    if (!name?.startsWith("--") || value === undefined) {
      throw new Error(`invalid argument near ${name ?? "end of command"}`);
    }
    options.set(name.slice(2), value);
  }
  return options;
}

function required(options, name) {
  const value = options.get(name)?.trim();
  if (!value) throw new Error(`--${name} is required`);
  return value;
}

export function buildReleaseNotes({ version, repository, previousTag, commits }) {
  const normalizedVersion = version.replace(/^v/, "");
  if (!VERSION_PATTERN.test(normalizedVersion)) {
    throw new Error(`invalid semantic version: ${version}`);
  }
  if (!/^[^/\s]+\/[^/\s]+$/.test(repository)) {
    throw new Error(`invalid GitHub repository: ${repository}`);
  }
  const changes = commits
    .filter(
      (commit) =>
        commit.subject.trim() &&
        !IGNORED_SUBJECTS.some((pattern) => pattern.test(commit.subject)),
    )
    .flatMap((commit) => [
      `- ${commit.subject} (\`${commit.hash}\`)`,
      ...(commit.details ?? []).map((detail) => `  - ${detail}`),
    ]);
  const range = previousTag ? `${previousTag}...v${normalizedVersion}` : `v${normalizedVersion}`;
  const lines = [
    `## Cursor BYOK v${normalizedVersion}`,
    "",
    ...(normalizedVersion.includes("-")
      ? ["> **Beta release.** This is a test version.", ""]
      : []),
    "### Changes",
    "",
    changes.length > 0 ? changes.join("\n") : "- Maintenance release.",
    "",
    "### Install",
    "",
    "Download the installer for your platform from the assets below.",
    "",
    ...(previousTag
      ? [`[Compare changes](https://github.com/${repository}/compare/${previousTag}...v${normalizedVersion})`, ""]
      : []),
  ];
  return { markdown: lines.join("\n"), range, changes };
}

function git(args) {
  return execFileSync("git", args, { encoding: "utf8" }).trim();
}

function previousTag(version) {
  try {
    return git(["describe", "--tags", "--abbrev=0", `v${version}^`]);
  } catch {
    return "";
  }
}

function commitsBetween(version, previous) {
  const range = previous ? `${previous}..v${version}` : `v${version}`;
  const output = git([
    "log",
    "--no-merges",
    "--pretty=format:%h%x1f%s%x1f%b%x1e",
    "-n",
    "100",
    range,
  ]);
  return output
    .split("\x1e")
    .map((record) => record.trim())
    .filter(Boolean)
    .map((record) => {
      const [hash, subject, body = ""] = record.split("\x1f");
      return {
        hash,
        subject,
        details: body
          .split("\n")
          .map((line) => line.trim())
          .filter((line) => /^[-*]\s+/.test(line))
          .map((line) => line.replace(/^[-*]\s+/, "")),
      };
    });
}

async function main() {
  const options = readOptions(process.argv.slice(2));
  const version = required(options, "version").replace(/^v/, "");
  const repository = required(options, "repository");
  const output = required(options, "output");
  if (!VERSION_PATTERN.test(version)) {
    throw new Error(`invalid semantic version: ${version}`);
  }
  if (!/^[^/\s]+\/[^/\s]+$/.test(repository)) {
    throw new Error(`invalid GitHub repository: ${repository}`);
  }
  const previous = options.get("previous-tag")?.trim() || previousTag(version);
  const notes = buildReleaseNotes({
    version,
    repository,
    previousTag: previous,
    commits: commitsBetween(version, previous),
  });
  await writeFile(output, notes.markdown);
  console.log(`wrote ${output} (${notes.changes.length} changes, range ${notes.range})`);
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) {
  main().catch((error) => {
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  });
}
