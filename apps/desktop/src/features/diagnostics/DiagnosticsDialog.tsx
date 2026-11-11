import { useEffect, useState } from "react";
import { api } from "../../shared/api";
import { diagnosticsStore, useDiagnosticsStore } from "../../shared/diagnostics/store";
import type { DiagnosticsReportDetail } from "../../shared/diagnostics/types";
import { Modal } from "../../shared/ui/Modal";
import { useMessage } from "../../shared/ui/message";
import styles from "./DiagnosticsDialog.module.scss";

export function DiagnosticsDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
  const { reports, busy, uploadingReportId, error } = useDiagnosticsStore();
  const message = useMessage();
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [detail, setDetail] = useState<DiagnosticsReportDetail | null>(null);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    if (!open) return;
    const fallback = reports[0]?.reportId ?? null;
    if (!selectedId || !reports.some((report) => report.reportId === selectedId)) {
      setSelectedId(fallback);
    }
  }, [open, reports, selectedId]);

  useEffect(() => {
    if (!open || !selectedId) {
      setDetail(null);
      return;
    }
    let active = true;
    setLoading(true);
    void diagnosticsStore.loadReport(selectedId)
      .then((next) => {
        if (active) setDetail(next);
      })
      .catch((cause) => {
        if (active) message(cause instanceof Error ? cause.message : String(cause));
      })
      .finally(() => {
        if (active) setLoading(false);
      });
    return () => {
      active = false;
    };
  }, [message, open, selectedId]);

  const createIssue = async () => {
    if (!selectedId || !detail?.uploadAvailable || busy) return;
    try {
      const preview = await diagnosticsStore.createIssue(selectedId);
      await api.openExternalUrl(preview.issueUrl);
      message(t("已上传诊断包并打开 GitHub，请检查后点击 Submit"));
      onClose();
    } catch (cause) {
      message(cause instanceof Error ? cause.message : String(cause));
    }
  };

  return <Modal
    open={open}
    title={t("检测到问题")}
    wide
    busy={busy}
    onClose={onClose}
    onSubmit={() => void createIssue()}
    submitLabel={t("上传并创建 Issue")}
    submitDisabled={!detail?.uploadAvailable || loading}
    secondaryAction={selectedId
      ? <button
          type="button"
          className={styles.dismiss}
          disabled={busy}
          onClick={() => {
            void diagnosticsStore.dismiss(selectedId).catch((cause) =>
              message(cause instanceof Error ? cause.message : String(cause)));
          }}
        >{t("忽略此问题")}</button>
      : undefined}
  >
    <div className={styles.layout}>
      <nav className={styles.list} aria-label={t("问题列表")}>
        {reports.map((report) => <button
          type="button"
          key={report.reportId}
          className={[styles.item, report.reportId === selectedId && styles.selected].filter(Boolean).join(" ")}
          onClick={() => setSelectedId(report.reportId)}
        >
          <span className={styles.code}>{report.code}</span>
          <span className={styles.summary}>{report.summary}</span>
          <span className={styles.meta}>
            {report.occurrences > 1 ? t("出现 {count} 次", { count: report.occurrences }) : report.severity}
          </span>
        </button>)}
        {reports.length === 0 && <span className={styles.empty}>{t("没有待处理的问题")}</span>}
      </nav>
      <section className={styles.detail}>
        {error && <div className={styles.error}>{error}</div>}
        {loading && <div className={styles.empty}>{t("加载中…")}</div>}
        {!loading && detail && <>
          <p className={styles.notice}>{t("创建 Issue 前会自动上传脱敏诊断包；未提交 Issue 时服务端会在 30 天后删除。本机不保留诊断包。")}</p>
          {!detail.uploadAvailable && <div className={styles.error}>{t("诊断上传服务未配置，暂时无法创建 Issue。")}</div>}
          <pre className={styles.preview} aria-label={t("Issue 正文预览")}>{detail.issueBody}</pre>
          <details className={styles.json}>
            <summary>{t("查看诊断 JSON")}</summary>
            <pre>{JSON.stringify(detail.diagnosticJson, null, 2)}</pre>
          </details>
        </>}
        {!loading && !detail && <div className={styles.empty}>{t("选择左侧问题查看详情")}</div>}
      </section>
    </div>
    {uploadingReportId && <span className={styles.uploading}>{t("正在上传诊断包…")}</span>}
  </Modal>;
}
