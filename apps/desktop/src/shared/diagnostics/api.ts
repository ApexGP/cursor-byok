import { request } from "../api";
import type {
  DiagnosticsIssuePreview,
  DiagnosticsReportDetail,
  DiagnosticsReportSummary,
  FrontendDiagnosticError,
} from "./types";

export function listDiagnosticsReports(): Promise<DiagnosticsReportSummary[]> {
  return request<DiagnosticsReportSummary[]>("/diagnostics/reports");
}

export function getDiagnosticsReport(reportId: string): Promise<DiagnosticsReportDetail> {
  return request<DiagnosticsReportDetail>(`/diagnostics/reports/${encodeURIComponent(reportId)}`);
}

export function createDiagnosticsIssue(reportId: string): Promise<DiagnosticsIssuePreview> {
  return request<DiagnosticsIssuePreview>(
    `/diagnostics/reports/${encodeURIComponent(reportId)}/issue`,
    { method: "POST" },
  );
}

export function dismissDiagnosticsReport(reportId: string): Promise<void> {
  return request<void>(`/diagnostics/reports/${encodeURIComponent(reportId)}/dismiss`, {
    method: "POST",
  });
}

export function reportFrontendError(input: FrontendDiagnosticError): Promise<void> {
  return request<void>("/diagnostics/frontend", {
    method: "POST",
    body: JSON.stringify(input),
  });
}
