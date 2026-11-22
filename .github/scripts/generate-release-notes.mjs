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

export function buildReleaseNotes({
  version,
  repository,
  previousTag,
  commits,
  pullRequests = [],
}) {
  const normalizedVersion = version.replace(/^v/, "");
  if (!VERSION_PATTERN.test(normalizedVersion)) {
    throw new Error(`invalid semantic version: ${version}`);
  }
  if (!/^[^/\s]+\/[^/\s]+$/.test(repository)) {
    throw new Error(`invalid GitHub repository: ${repository}`);
  }
  const pullRequestIndex = pullRequestCommitIndex(pullRequests);
  const changes = commits
    .filter(
      (commit) =>
        commit.subject.trim() &&
        !IGNORED_SUBJECTS.some((pattern) => pattern.test(commit.subject)),
    )
    .flatMap((commit) => {
      const pullRequest = matchingPullRequest(commit, pullRequestIndex);
      const author = pullRequest ? pullRequestAuthor(pullRequest) : "";
      const thanks = author ? ` — thanks @${author}` : "";
      return [
        `- ${commit.subject} (\`${commit.hash}\`)${thanks}`,
        ...(commit.details ?? []).map((detail) => `  - ${detail}`),
      ];
    });
  const contributors = contributorLines(repository, pullRequests);
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
    ...(contributors.length > 0
      ? ["### Thanks to contributors", "", ...contributors, ""]
      : []),
    "### Install",
    "",
    "Download the installer for your platform from the assets below.",
    "",
    ...(previousTag
      ? [`[Compare changes](https://github.com/${repository}/compare/${previousTag}...v${normalizedVersion})`, ""]
      : []),
  ];
  return { markdown: lines.join("\n"), range, changes, contributors };
}

function pullRequestAuthor(pullRequest) {
  const raw =
    typeof pullRequest.author === "string"
      ? pullRequest.author
      : pullRequest.author?.login;
  const login = typeof raw === "string" ? raw.trim() : "";
  return /^[A-Za-z0-9-]+$/.test(login) ? login : "";
}

function pullRequestCommitIndex(pullRequests) {
  const index = [];
  for (const pullRequest of pullRequests) {
    if (!pullRequestAuthor(pullRequest)) continue;
    for (const sha of pullRequest.commits ?? []) {
      if (typeof sha === "string" && sha.length >= 7) {
        index.push({ sha, pullRequest });
      }
    }
  }
  return index;
}

function matchingPullRequest(commit, index) {
  const fullHash = commit.fullHash ?? commit.hash;
  return (
    index.find(
      ({ sha }) =>
        sha === fullHash ||
        (commit.hash && sha.startsWith(commit.hash)) ||
        (fullHash && fullHash.startsWith(sha)),
    )?.pullRequest ?? null
  );
}

function contributorLines(repository, pullRequests) {
  const byAuthor = new Map();
  for (const pullRequest of pullRequests) {
    const author = pullRequestAuthor(pullRequest);
    if (!author || !Number.isInteger(pullRequest.number)) continue;
    const entries = byAuthor.get(author) ?? [];
    entries.push(pullRequest);
    byAuthor.set(author, entries);
  }
  return [...byAuthor.entries()].map(([author, entries]) => {
    const links = entries
      .map((pullRequest) => {
        const url =
          typeof pullRequest.url === "string" &&
          pullRequest.url.startsWith(`https://github.com/${repository}/pull/`)
            ? pullRequest.url
            : `https://github.com/${repository}/pull/${pullRequest.number}`;
        return `[#${pullRequest.number}](${url})`;
      })
      .join(", ");
    const titles = [
      ...new Set(
        entries
          .map((pullRequest) =>
            typeof pullRequest.title === "string" ? pullRequest.title.trim() : "",
          )
          .filter(Boolean),
      ),
    ];
    return `- @${author} for ${links}${titles.length === 1 ? ` — ${titles[0]}` : ""}`;
  });
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
    "--pretty=format:%H%x1f%h%x1f%s%x1f%b%x1e",
    "-n",
    "100",
    range,
  ]);
  return output
    .split("\x1e")
    .map((record) => record.trim())
    .filter(Boolean)
    .map((record) => {
      const [fullHash, hash, subject, body = ""] = record.split("\x1f");
      return {
        fullHash,
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

function gh(args) {
  return execFileSync("gh", args, { encoding: "utf8" }).trim();
}

function commitsInRange(version, previous) {
  const range = previous ? `${previous}..v${version}` : `v${version}`;
  return new Set(
    git(["rev-list", range])
      .split("\n")
      .map((line) => line.trim())
      .filter(Boolean),
  );
}

function mergedPullRequests(repository, version, previous) {
  if (!previous) return [];
  const rangeHashes = commitsInRange(version, previous);
  let listed;
  try {
    listed = JSON.parse(
      gh([
        "pr",
        "list",
        "--repo",
        repository,
        "--state",
        "merged",
        "--limit",
        "200",
        "--json",
        "number,title,author,mergeCommit,url",
      ]),
    );
  } catch (error) {
    console.warn(
      `skipping contributor credits: ${error instanceof Error ? error.message : String(error)}`,
    );
    return [];
  }
  return listed
    .filter(
      (pullRequest) =>
        pullRequest.mergeCommit?.oid && rangeHashes.has(pullRequest.mergeCommit.oid),
    )
    .map((pullRequest) => {
      let commits = [];
      try {
        const details = JSON.parse(
          gh([
            "pr",
            "view",
            String(pullRequest.number),
            "--repo",
            repository,
            "--json",
            "commits",
          ]),
        );
        commits = (details.commits ?? [])
          .map((commit) => commit.oid)
          .filter(Boolean);
      } catch (error) {
        console.warn(
          `could not read commits for #${pullRequest.number}: ${error instanceof Error ? error.message : String(error)}`,
        );
      }
      return {
        number: pullRequest.number,
        title: pullRequest.title,
        url: pullRequest.url,
        author: pullRequest.author,
        commits: [pullRequest.mergeCommit.oid, ...commits],
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
  const pullRequests = mergedPullRequests(repository, version, previous);
  const notes = buildReleaseNotes({
    version,
    repository,
    previousTag: previous,
    commits: commitsBetween(version, previous),
    pullRequests,
  });
  await writeFile(output, notes.markdown);
  console.log(
    `wrote ${output} (${notes.changes.length} changes, ${pullRequests.length} pull requests, range ${notes.range})`,
  );
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) {
  main().catch((error) => {
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  });
}
