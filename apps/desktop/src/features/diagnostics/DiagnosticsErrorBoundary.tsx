import { Component, type ErrorInfo, type ReactNode } from "react";
import { diagnosticsStore } from "../../shared/diagnostics/store";

type Props = { children: ReactNode };
type State = { failed: boolean };

export class DiagnosticsErrorBoundary extends Component<Props, State> {
  state: State = { failed: false };

  static getDerivedStateFromError(): State {
    return { failed: true };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    diagnosticsStore.reportRendererError({
      code: "CB-UI-001",
      message: error.message || "renderer error",
      stack: info.componentStack ?? error.stack,
    });
  }

  render() {
    if (this.state.failed) {
      return <div style={{ padding: 24 }}>
        <strong>{t("界面渲染失败")}</strong>
        <p>{t("可点击右下角“检测到问题”查看并反馈诊断信息。")}</p>
      </div>;
    }
    return this.props.children;
  }
}
