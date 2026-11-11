import {
  expiresAt,
  objectKey,
  validateReportId,
  validateSha256,
  validateZip,
} from "./validate.js";

const SCHEMA_VERSION = "1";

export default {
  async fetch(request, env, ctx) {
    const url = new URL(request.url);
    if (request.method === "POST" && url.pathname === "/v1/reports") {
      return upload(request, env);
    }
    if (request.method === "GET" && url.pathname.startsWith("/r/")) {
      return download(request, env, url.pathname.slice("/r/".length));
    }
    return json({ message: "not found" }, 404);
  },
};

async function upload(request, env) {
  const reportId = request.headers.get("x-report-id");
  const sha256 = request.headers.get("x-report-sha256");
  const schemaVersion = request.headers.get("x-schema-version");
  if (!validateReportId(reportId)) return json({ message: "invalid report id" }, 400);
  if (!validateSha256(sha256)) return json({ message: "invalid report sha256" }, 400);
  if (schemaVersion !== SCHEMA_VERSION) return json({ message: "unsupported schema" }, 400);
  if (request.headers.get("content-type")?.split(";")[0] !== "application/zip") {
    return json({ message: "content-type must be application/zip" }, 415);
  }

  const maxBytes = Number(env.MAX_BUNDLE_BYTES ?? 8 * 1024 * 1024);
  const declared = Number(request.headers.get("content-length") ?? NaN);
  if (Number.isFinite(declared) && declared > maxBytes) {
    return json({ message: "bundle too large" }, 413);
  }
  const bytes = new Uint8Array(await request.arrayBuffer());
  if (bytes.byteLength > maxBytes) return json({ message: "bundle too large" }, 413);
  if (!validateZip(bytes)) return json({ message: "body is not a zip" }, 415);
  if ((await sha256Hex(bytes)) !== sha256) return json({ message: "sha256 mismatch" }, 400);

  const key = objectKey(reportId);
  if (await env.DIAGNOSTICS_BUCKET.head(key)) {
    return json({ message: "report already exists" }, 409);
  }
  await env.DIAGNOSTICS_BUCKET.put(key, bytes, {
    httpMetadata: { contentType: "application/zip" },
    customMetadata: {
      schemaVersion,
      appVersion: request.headers.get("x-app-version") ?? "",
      platform: request.headers.get("x-platform") ?? "",
    },
  });

  const retentionDays = Number(env.RETENTION_DAYS ?? 30);
  const expires = expiresAt(new Date(), retentionDays);
  const base = (env.PUBLIC_BASE_URL ?? new URL(request.url).origin).replace(/\/$/, "");
  return json({ id: reportId, url: `${base}/r/${reportId}`, expiresAt: expires.toISOString() }, 201);
}

async function download(request, env, reportId) {
  if (!validateReportId(reportId)) return json({ message: "invalid report id" }, 400);
  const object = await env.DIAGNOSTICS_BUCKET.get(objectKey(reportId));
  if (!object) return json({ message: "not found" }, 404);
  const headers = new Headers({
    "content-type": "application/zip",
    "content-disposition": `attachment; filename="${reportId}.zip"`,
    "cache-control": "private, max-age=0",
    "x-robots-tag": "noindex",
  });
  return new Response(object.body, { status: 200, headers });
}

async function sha256Hex(bytes) {
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return [...new Uint8Array(digest)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
}

function json(value, status) {
  return new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json", "cache-control": "no-store" },
  });
}
