const REPORT_ID = /^r-\d{8}-[0-9a-f]{32}$/;
const SHA256 = /^[0-9a-f]{64}$/;
const ZIP_MAGIC = new Uint8Array([0x50, 0x4b, 0x03, 0x04]);

export function validateReportId(value) {
  return typeof value === "string" && REPORT_ID.test(value);
}

export function validateSha256(value) {
  return typeof value === "string" && SHA256.test(value);
}

export function validateZip(bytes) {
  if (!(bytes instanceof Uint8Array) || bytes.length < ZIP_MAGIC.length) return false;
  return ZIP_MAGIC.every((byte, index) => bytes[index] === byte);
}

export function objectKey(reportId) {
  if (!validateReportId(reportId)) throw new Error("invalid report id");
  return `reports/${reportId}.zip`;
}

export function expiresAt(now, retentionDays) {
  return new Date(now.getTime() + retentionDays * 24 * 60 * 60 * 1000);
}
