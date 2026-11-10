import { useCallback, useEffect, useState } from "react";
import { PageContent } from "../../shell/layout/PageContent";
import { api, type ConversationSummary } from "../../shared/api";
import { useMessage } from "../../shared/ui/message";
import styles from "./ConversationsPage.module.scss";

function folderName(path: string): string {
  const trimmed = path.replace(/[\\/]+$/, "");
  return trimmed.split(/[\\/]/).pop() ?? "";
}

function labelOf(item: ConversationSummary): string {
  return item.title || item.preview || item.conversation_id;
}

function metaOf(item: ConversationSummary): string {
  const parts: string[] = [];
  const folder = folderName(item.workspace);
  if (folder) parts.push(folder);
  if (item.subagent) parts.push(t("子智能体"));
  parts.push(item.conversation_id.slice(0, 8));
  return parts.join(" · ");
}

export function ConversationsPage() {
  const [items, setItems] = useState<ConversationSummary[]>([]);
  const [pending, setPending] = useState<string | null>(null);
  const showMessage = useMessage();

  const refresh = useCallback(() => {
    void api
      .conversations()
      .then(setItems)
      .catch((error: unknown) => {
        showMessage(error instanceof Error ? error.message : String(error));
      });
  }, [showMessage]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const remove = async (item: ConversationSummary) => {
    const label = labelOf(item);
    if (
      !window.confirm(
        t("删除后本地不再保留这段对话的数据；若 Cursor 已退出，侧边栏条目也会一并删除。确定删除？") +
          "\n" +
          label,
      )
    ) {
      return;
    }
    setPending(item.conversation_id);
    try {
      const report = await api.deleteConversation(item.conversation_id);
      const hadLocalData = report.messages > 0 || report.checkpoints > 0;
      let text = hadLocalData
        ? t("已删除该会话的本地数据") +
          ` · ${report.messages} ${t("条消息")} / ${report.checkpoints} ${t("个快照")}`
        : t("本地已无该会话的数据");
      if (report.cursor_rows === null) {
        text += t("；Cursor 仍在运行，侧边栏条目未删除，退出 Cursor 后再删一次");
      } else if (report.cursor_rows > 0) {
        text += t("；Cursor 侧边栏条目已同步移除");
      } else if (hadLocalData) {
        text += t("；Cursor 侧未找到对应条目");
      }
      showMessage(text);
      refresh();
    } catch (error: unknown) {
      showMessage(error instanceof Error ? error.message : String(error));
    } finally {
      setPending(null);
    }
  };

  const content = (
    <div className={styles.page}>
      <div className={styles.header}>
        <span className={styles.count}>{items.length}</span>
        <span className={styles.hint}>{t("按最后使用时间排序")}</span>
      </div>
      <div className={styles.table}>
        <div className={styles.row + " " + styles.head}>
          <span>{t("对话")}</span>
          <span className={styles.num}>{t("消息")}</span>
          <span className={styles.num}>{t("快照")}</span>
          <span className={styles.time}>{t("最后使用")}</span>
          <span className={styles.action} />
        </div>
        {items.map((item) => (
          <div className={styles.row} key={item.conversation_id}>
            <span className={styles.preview} title={`${labelOf(item)}\n${item.conversation_id}`}>
              <span className={styles.title}>{labelOf(item)}</span>
              <span className={styles.meta}>{metaOf(item)}</span>
            </span>
            <span className={styles.num}>{item.message_count}</span>
            <span className={styles.num}>{item.checkpoint_count}</span>
            <span className={styles.time}>
              {new Date(item.last_used_ms).toLocaleString()}
            </span>
            <span className={styles.action}>
              <button
                type="button"
                className={styles.delete}
                disabled={pending === item.conversation_id}
                onClick={() => void remove(item)}
              >
                {pending === item.conversation_id ? t("删除中") : t("删除")}
              </button>
            </span>
          </div>
        ))}
        {items.length === 0 && <div className={styles.empty}>{t("暂无会话数据")}</div>}
      </div>
    </div>
  );

  return (
    <PageContent
      fixed
      title={t("会话数据")}
      contentClassName={styles.pageContent}
      sections={[{ key: "conversations", estimatedHeight: 720, content }]}
    />
  );
}
