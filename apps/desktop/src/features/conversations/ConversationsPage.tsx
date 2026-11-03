import { useCallback, useEffect, useState } from "react";
import { PageContent } from "../../shell/layout/PageContent";
import { api, type ConversationSummary } from "../../shared/api";
import { useMessage } from "../../shared/ui/message";
import styles from "./ConversationsPage.module.scss";

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
    const label = item.preview || item.conversation_id.slice(0, 8);
    if (!window.confirm(t("删除后本地不再保留这段对话的数据，Cursor 侧再次打开会重新导入。确定删除？") + "\n" + label)) {
      return;
    }
    setPending(item.conversation_id);
    try {
      const report = await api.deleteConversation(item.conversation_id);
      showMessage(
        t("已删除该会话的本地数据")
          + ` · ${report.messages} ${t("条消息")} / ${report.checkpoints} ${t("个快照")}`,
      );
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
            <span className={styles.preview} title={item.conversation_id}>
              {item.preview || item.conversation_id.slice(0, 8)}
              <em className={styles.id}>{item.conversation_id.slice(0, 8)}</em>
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
