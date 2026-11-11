import { useState } from "react";
import { useDiagnosticsStore } from "../../shared/diagnostics/store";
import { DiagnosticsDialog } from "./DiagnosticsDialog";
import styles from "./DiagnosticsButton.module.scss";

export function DiagnosticsButton() {
  const { reports } = useDiagnosticsStore();
  const [open, setOpen] = useState(false);
  if (reports.length === 0 && !open) return null;
  return <>
    <button
      type="button"
      className={styles.root}
      onClick={() => setOpen(true)}
      aria-label={t("检测到 {count} 个问题", { count: reports.length })}
    >
      <span className={styles.dot} aria-hidden="true" />
      <span>{t("检测到问题")}</span>
      <span className={styles.count}>{reports.length}</span>
    </button>
    <DiagnosticsDialog open={open} onClose={() => setOpen(false)} />
  </>;
}
