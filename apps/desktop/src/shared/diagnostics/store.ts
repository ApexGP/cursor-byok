import { useSyncExternalStore } from "react";
import {
  createDiagnosticsIssue,
  dismissDiagnosticsReport,
  getDiagnosticsReport,
  listDiagnosticsReports,
  reportFrontendError,
} from "./api";
import type {
  DiagnosticsIssuePreview,
  DiagnosticsReportDetail,
  DiagnosticsReportSummary,
  FrontendDiagnosticError,
} from "./types";

const POLL_INTERVAL_MS = 10_000;

export interface DiagnosticsSnapshot {
  reports: DiagnosticsReportSummary[];
  busy: boolean;
  uploadingReportId: string | null;
  error: string | null;
}

let snapshot: DiagnosticsSnapshot = {
  reports: [],
  busy: false,
  uploadingReportId: null,
  error: null,
};

const listeners = new Set<() => void>();
let pollTimer: number | null = null;
let refreshInFlight = false;

function update(patch: Partial<DiagnosticsSnapshot>) {
  snapshot = { ...snapshot, ...patch };
  listeners.forEach((listener) => listener());
}

function errorText(cause: unknown) {
  return cause instanceof Error ? cause.message : String(cause);
}

async function refresh() {
  if (refreshInFlight || document.visibilityState === "hidden") return;
  refreshInFlight = true;
  try {
    const reports = await listDiagnosticsReports();
    update({ reports, error: null });
  } catch (cause) {
    update({ error: errorText(cause) });
  } finally {
    refreshInFlight = false;
  }
}

function startPolling() {
  if (pollTimer !== null) return;
  void refresh();
  pollTimer = window.setInterval(() => void refresh(), POLL_INTERVAL_MS);
}

function stopPolling() {
  if (pollTimer === null) return;
  window.clearInterval(pollTimer);
  pollTimer = null;
}

async function loadReport(reportId: string): Promise<DiagnosticsReportDetail> {
  return getDiagnosticsReport(reportId);
}

async function createIssue(reportId: string): Promise<DiagnosticsIssuePreview> {
  update({ busy: true, uploadingReportId: reportId, error: null });
  try {
    const preview = await createDiagnosticsIssue(reportId);
    update({
      reports: snapshot.reports.filter((report) => report.reportId !== reportId),
      error: null,
    });
    return preview;
  } catch (cause) {
    update({ error: errorText(cause) });
    throw cause;
  } finally {
    update({ busy: false, uploadingReportId: null });
  }
}

async function dismiss(reportId: string) {
  update({ busy: true, error: null });
  try {
    await dismissDiagnosticsReport(reportId);
    update({ reports: snapshot.reports.filter((report) => report.reportId !== reportId) });
  } catch (cause) {
    update({ error: errorText(cause) });
    throw cause;
  } finally {
    update({ busy: false });
  }
}

export function reportRendererError(input: FrontendDiagnosticError) {
  void reportFrontendError(input).catch(() => {
    // Reporting must never make the renderer error worse.
  });
}

export const diagnosticsStore = {
  subscribe(listener: () => void) {
    listeners.add(listener);
    return () => listeners.delete(listener);
  },
  getSnapshot: () => snapshot,
  refresh,
  loadReport,
  createIssue,
  dismiss,
  reportRendererError,
  startPolling,
  stopPolling,
};

export function useDiagnosticsStore() {
  return useSyncExternalStore(
    diagnosticsStore.subscribe,
    diagnosticsStore.getSnapshot,
    diagnosticsStore.getSnapshot,
  );
}
