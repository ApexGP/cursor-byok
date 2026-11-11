export type DiagnosticsSeverity = "warning" | "error" | "fatal";

export interface DiagnosticsReportSummary {
  reportId: string;
  code: string;
  severity: DiagnosticsSeverity;
  summary: string;
  occurrences: number;
  firstSeen: string;
  lastSeen: string;
  dismissed: boolean;
}

export interface DiagnosticsReportDetail {
  summary: DiagnosticsReportSummary;
  issueBody: string;
  diagnosticJson: unknown;
  uploadAvailable: boolean;
}

export interface DiagnosticsIssuePreview {
  title: string;
  body: string;
  labels: string[];
  issueUrl: string;
  bundleUrl: string;
  expiresAt: string;
}

export interface FrontendDiagnosticError {
  code: "CB-UI-001" | "CB-UI-002";
  message: string;
  stack?: string;
}
