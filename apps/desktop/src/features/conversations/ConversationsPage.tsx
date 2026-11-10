import { useCallback, useEffect, useMemo, useState } from "react";
import { PageContent } from "../../shell/layout/PageContent";
import { api, type ConversationSummary } from "../../shared/api";
import { useMessage } from "../../shared/ui/message";
import styles from "./ConversationsPage.module.scss";

const UNGROUPED = "__ungrouped__";

interface Group {
  key: string;
  /** Folder name shown as the project heading. */
  label: string;
  /** Full workspace path, kept for the tooltip. */
  path: string;
  items: ConversationSummary[];
  lastUsed: number;
}

function folderName(path: string): string {
  const trimmed = path.replace(/[\\/]+$/, "");
  const last = trimmed.split(/[\\/]/).pop() ?? "";
  // Multi-root windows are labelled by their `.code-workspace` file; Cursor
  // shows that name without the extension.
  return last.replace(/\.code-workspace$/i, "");
}

function labelOf(item: ConversationSummary): string {
  return item.title || item.preview || item.conversation_id;
}

function metaOf(item: ConversationSummary): string {
  const parts: string[] = [];
  if (item.subagent) parts.push(t("子智能体"));
  parts.push(item.conversation_id.slice(0, 8));
  return parts.join(" · ");
}

/** Cursor groups chats by workspace folder; mirror that so a project can be scanned at a glance. */
function groupConversations(items: ConversationSummary[]): Group[] {
  const map = new Map<string, Group>();
  for (const item of items) {
    const path = item.workspace.trim();
    const key = path || UNGROUPED;
    let group = map.get(key);
    if (!group) {
      group = {
        key,
        label: path ? folderName(path) : t("未归类"),
        path,
        items: [],
        lastUsed: 0,
      };
      map.set(key, group);
    }
    group.items.push(item);
    if (item.last_used_ms > group.lastUsed) group.lastUsed = item.last_used_ms;
  }
  const groups = [...map.values()];
  for (const group of groups) {
    group.items.sort((a, b) => b.last_used_ms - a.last_used_ms);
  }
  groups.sort((a, b) => b.lastUsed - a.lastUsed);
  return groups;
}

export function ConversationsPage() {
  const [items, setItems] = useState<ConversationSummary[]>([]);
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>({});
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

  const groups = useMemo(() => groupConversations(items), [items]);

  const toggleGroup = (key: string) => {
    setCollapsed((prev) => ({ ...prev, [key]: !prev[key] }));
  };

  const removeLocal = async (item: ConversationSummary) => {
    if (
      !window.confirm(
        t("删除后本地不再保留这段对话的数据。Cursor 侧边栏条目会保留，如需一并移除请先点「清侧边栏」。确定删除？") +
          "\n" +
          labelOf(item),
      )
    ) {
      return;
    }
    setPending(item.conversation_id);
    try {
      const report = await api.deleteConversation(item.conversation_id);
      showMessage(
        t("已删除该会话的本地数据") +
          ` · ${report.messages} ${t("条消息")} / ${report.checkpoints} ${t("个快照")}`,
      );
      refresh();
    } catch (error: unknown) {
      showMessage(error instanceof Error ? error.message : String(error));
    } finally {
      setPending(null);
    }
  };

  const removeSidebar = async (item: ConversationSummary) => {
    if (
      !window.confirm(
        t("这会直接修改 Cursor 的数据库，删除该对话在侧边栏的条目（含对话内容与气泡数据）。请先完全退出 Cursor，否则修改会被写回。确定继续？") +
          "\n" +
          labelOf(item),
      )
    ) {
      return;
    }
    setPending(item.conversation_id);
    try {
      const report = await api.deleteSidebarEntry(item.conversation_id);
      showMessage(
        report.found
          ? t("已移除 Cursor 侧边栏条目") + ` · ${report.removed} ${t("行")}`
          : t("Cursor 侧没有找到这条对话的条目；若 Cursor 仍在运行，请退出后再试"),
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
        <span className={styles.hint}>
          {t("按项目分组")} · {t("先完全退出 Cursor 再清侧边栏")}
        </span>
      </div>
      <div className={styles.table}>
        <div className={styles.row + " " + styles.head}>
          <span>{t("对话")}</span>
          <span className={styles.num}>{t("消息")}</span>
          <span className={styles.num}>{t("快照")}</span>
          <span className={styles.time}>{t("最后使用")}</span>
          <span className={styles.action} />
        </div>
        {groups.map((group) => {
          const isCollapsed = Boolean(collapsed[group.key]);
          return (
            <div className={styles.group} key={group.key}>
              <button
                type="button"
                className={styles.groupHead}
                title={group.path || t("未归类")}
                onClick={() => toggleGroup(group.key)}
              >
                <span className={styles.caret}>{isCollapsed ? "▸" : "▾"}</span>
                <span className={styles.folder} aria-hidden="true">
                  {isCollapsed ? "📁" : "📂"}
                </span>
                <span className={styles.groupName}>{group.label}</span>
                <span className={styles.groupCount}>{group.items.length}</span>
                <span className={styles.groupTime}>
                  {new Date(group.lastUsed).toLocaleString()}
                </span>
              </button>
              {!isCollapsed &&
                group.items.map((item) => (
                  <div className={styles.row + " " + styles.item} key={item.conversation_id}>
                    <span
                      className={styles.preview}
                      title={`${labelOf(item)}\n${item.conversation_id}`}
                    >
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
                        className={styles.sidebar}
                        disabled={pending === item.conversation_id}
                        onClick={() => void removeSidebar(item)}
                      >
                        {t("清侧边栏")}
                      </button>
                      <button
                        type="button"
                        className={styles.delete}
                        disabled={pending === item.conversation_id}
                        onClick={() => void removeLocal(item)}
                      >
                        {pending === item.conversation_id ? t("删除中") : t("删除")}
                      </button>
                    </span>
                  </div>
                ))}
            </div>
          );
        })}
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
