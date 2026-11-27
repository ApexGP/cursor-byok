# Security Policy

## Supported versions

Security fixes are applied to the latest published release. Older releases may
not receive backports. Always test a report against the latest release first.

## Reporting a vulnerability

Do not open a public issue, discussion, or pull request for a suspected
vulnerability.

Use **_Report a vulnerability_** in the repository's **Security** tab so the
report stays private. If private reporting is unavailable, contact the
maintainer through the profile linked from `README.md` using a private channel.

Include:

- the affected version, operating system, and installation type;
- the impact and the trust boundary that is crossed;
- minimal reproduction steps or a proof of concept;
- whether credentials, conversation contents, or local files may have been
  exposed.

Never include real API keys, Cursor tokens, personal paths, or database dumps.
If a credential may have leaked, revoke it with the provider before reporting.
Maintainers will acknowledge the report, investigate, and coordinate disclosure
and credit with you. Do not publish details until a fix or mitigation is
available.

## Security model

cursor-byok runs on the user's machine and intentionally sits between the
Cursor client and the configured model provider. The security model therefore
focuses on preserving local trust boundaries and credentials, not on defending
against a compromised local account.

### Local interception and the CA

The desktop app installs a local MITM proxy and a locally generated CA so Cursor
can be routed without a custom base URL. Only managed Cursor hosts are
intercepted; other CONNECT traffic is passed through. The
`x-server-upstream-url` header is accepted only for managed HTTPS hosts, so the
proxy cannot be used as a general SSRF pivot.

The CA private key and generated certificates are local secrets. They must never
be committed, logged, included in diagnostics, or shared. Weakening the managed
host allowlist or the header validation is a security change and requires
review.

### Credentials and local data

Model API keys, custom headers, and application settings are stored in the local
data directory (`~/.cursor-byok-v3/`, mode `0700` on Unix). Keys must not appear
in logs, error messages, diagnostic bundles, issue bodies, crash reports, or
test fixtures. Provider keys and Authorization headers are covered by the
diagnostics redactor; do not bypass it.

### Cursor account and tokens

The local app creates a local Cursor-compatible token and forwards official
Cursor tokens unchanged. If account or cache code reads a JWT `sub` claim, that
claim must be treated as an unverified identity hint at most, never as
authorization. Any change to token handling, JWT parsing, account identity, or
the Statsig bootstrap requires security review.

### Cursor-owned state

The app reads Cursor's local catalog and settings, and may need to repair
Cursor-owned state in narrow cases. Cursor's `state.vscdb` and `settings.json`
are not part of this project's schema. Any write to them must:

- happen only while Cursor is stopped or through a documented, guarded path;
- be backed up or atomically replaceable before the first write;
- preserve unrelated keys and unknown fields;
- be covered by a rollback test and a clear explanation of why Cursor's own
  cleanup is insufficient.

The composer `chatGenerationUUID` repair is the current write path: it runs
while Cursor is stopped, snapshots `state.vscdb` and its WAL/SHM sidecars before
writing, preserves unknown fields, and commits the updates in one transaction.

A write to Cursor-owned state without backup or a guarded lifecycle is a
security and data-loss risk.

### Tools, MCP, and subagents

Tool calls execute with the user's privileges on the user's machine. The admin
command denylist is a safety control, not a sandbox. MCP servers and subagents
inherit the same boundary. Changes that add a new execution path, weaken an
approval, or allow a remote party to choose arbitrary commands require security
review. The former plugin runtime and external compatibility API were removed;
do not reintroduce a plugin or unauthenticated proxy surface without a new
design.

### Control plane

The desktop control plane binds to loopback and serves the embedded frontend
under `/__byok-api__/`. It has no multi-user authentication. The Docker image
exposes the control plane on `0.0.0.0:3000` by design; operators must put it
behind their own network controls and must not expose it to untrusted networks.

New control-plane routes must stay on the local management surface and must not
proxy arbitrary user-supplied URLs.

### Diagnostics and capture

Diagnostics are opt-in. Captured text is redacted, database evidence is limited
to an allowlist of fields, and the bundle is generated in memory. Uploads happen
only after the user confirms; the relay keeps a bundle for 30 days when no issue
is submitted. Do not add message bodies, request/response bodies, credentials,
or full databases to a bundle.

Bidi decode-failure dumps may contain conversation content. They are written
only when decoding fails, live under the protected data directory, and must not
be uploaded or attached automatically.

### Updates and releases

Updates are delivered through release manifests. Tauri `latest.json` and the
Windows portable manifest carry signatures; the legacy `update.json` carries
SHA-256 checksums. Mixing the three formats can serve the wrong asset. Releases
are owner-gated and must use the repository's release workflow. Do not add an
unsigned or unpinned update path.

## Security review triggers

Request a security review when a change does any of the following:

- adds a new outbound network destination, redirects existing traffic, or
  bypasses the managed-host proxy boundary;
- reads, stores, logs, or transmits credentials, tokens, or request bodies;
- adds or changes process execution, shell command construction, MCP routing,
  or subagent execution;
- writes outside the app data directory or into Cursor-owned files;
- weakens the redactor, diagnostics allowlist, CSP, Tauri capabilities, or
  managed-host allowlist;
- adds an unauthenticated local or remote endpoint;
- changes JWT, account, Statsig, or updater handling;
- introduces a new persistence path for conversation or provider payloads.

Include the trust boundary, the new capability, and the tests that prove the
boundary in the PR description.

## Out of scope

- Attacks that require an already compromised local account or administrator
  access.
- A user deliberately configuring a malicious model provider, MCP server, or
  upstream endpoint; the provider is chosen by the user.
- Vulnerabilities in Cursor itself, in third-party model providers, or in
  operating-system components.
- Denial of service caused solely by a user's own local configuration.
