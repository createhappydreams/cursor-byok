import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { PageContent } from "../../shell/layout/PageContent";
import {
  api,
  type ConversationSummary,
  type CursorSideRemoval,
  type EverywhereRemoval,
  type PurgedConversation,
  type SelectionRemoval,
} from "../../shared/api";
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

function timeOf(ms: number): string {
  // Chats that only exist in Cursor's sidebar carry no timestamp.
  return ms > 0 ? new Date(ms).toLocaleString() : "—";
}

/** Human summary of what was removed from Cursor's own database. */
function cursorSummary(report: CursorSideRemoval): string {
  const parts: string[] = [];
  if (report.bubbles) parts.push(`${report.bubbles} ${t("气泡")}`);
  if (report.conversation_data) parts.push(`${report.conversation_data} ${t("会话文档")}`);
  if (report.conversation_headers) parts.push(`${report.conversation_headers} ${t("列表条目")}`);
  if (report.checkpoints) parts.push(`${report.checkpoints} ${t("快照")}`);
  if (report.file_caches) parts.push(`${report.file_caches} ${t("文件缓存")}`);
  if (report.request_contexts) parts.push(`${report.request_contexts} ${t("请求上下文")}`);
  if (report.ui_state) parts.push(`${report.ui_state} ${t("界面状态")}`);
  if (!parts.length) parts.push(`0 ${t("行")}`);
  const removed = `${t("Cursor 侧已删")} · ${parts.join(" / ")}`;
  if (report.remaining > 0) {
    return `${removed}\n${t("⚠ 仍有")} ${report.remaining} ${t(
      "行残留：Cursor 可能还在运行并把数据写回了，请完全退出后重试",
    )}`;
  }
  return `${removed} · ${t("已彻底移除")}`;
}

/** Local row counts, shared by the single and batch summaries. */
function localSummary(report: PurgedConversation): string {
  return (
    `${report.messages} ${t("条消息")} / ${report.checkpoints} ${t("个快照")}` +
    ` / ${report.runs} ${t("次运行")} / ${report.llm_calls} ${t("次模型调用")}` +
    ` / ${report.blobs} ${t("个内容块")}`
  );
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

/** A header checkbox that can also render the mixed state of a partial selection. */
function CheckBox({
  checked,
  indeterminate = false,
  disabled = false,
  title,
  onChange,
}: {
  checked: boolean;
  indeterminate?: boolean;
  disabled?: boolean;
  title?: string;
  onChange: (checked: boolean) => void;
}) {
  const ref = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (ref.current) ref.current.indeterminate = indeterminate && !checked;
  }, [indeterminate, checked]);
  return (
    <input
      ref={ref}
      type="checkbox"
      className={styles.check}
      checked={checked}
      disabled={disabled}
      title={title}
      onChange={(event) => onChange(event.target.checked)}
    />
  );
}

export function ConversationsPage() {
  const [items, setItems] = useState<ConversationSummary[]>([]);
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>({});
  const [pending, setPending] = useState<string | null>(null);
  const [bulkBusy, setBulkBusy] = useState(false);
  const [selected, setSelected] = useState<Set<string>>(() => new Set());
  const showMessage = useMessage();

  const refresh = useCallback(() => {
    void api
      .conversations()
      .then((next) => {
        setItems(next);
        // Drop ids that vanished so the counter never overstates the selection.
        setSelected((prev) => {
          if (!prev.size) return prev;
          const alive = new Set(next.map((item) => item.conversation_id));
          const kept = [...prev].filter((id) => alive.has(id));
          return kept.length === prev.size ? prev : new Set(kept);
        });
      })
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

  const toggleOne = (id: string, checked: boolean) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (checked) next.add(id);
      else next.delete(id);
      return next;
    });
  };

  const toggleMany = (ids: string[], checked: boolean) => {
    setSelected((prev) => {
      const next = new Set(prev);
      for (const id of ids) {
        if (checked) next.add(id);
        else next.delete(id);
      }
      return next;
    });
  };

  const allSelected = items.length > 0 && selected.size === items.length;
  const selectedIds = useMemo(() => [...selected], [selected]);

  const removeLocal = async (item: ConversationSummary) => {
    if (
      !window.confirm(
        t("删除后本地不再保留这段对话的数据。Cursor 侧边栏条目会保留，如需一并移除请点「彻底删除」。确定删除？") +
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
        t("这会直接修改 Cursor 的数据库，删除该对话在侧边栏的全部数据（气泡、快照、文件缓存、列表条目）。请先完全退出 Cursor，否则修改会被写回。确定继续？") +
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
          ? cursorSummary(report)
          : t("Cursor 侧没有找到这条对话的条目；若 Cursor 仍在运行，请退出后再试"),
      );
      refresh();
    } catch (error: unknown) {
      showMessage(error instanceof Error ? error.message : String(error));
    } finally {
      setPending(null);
    }
  };

  const removeEverywhere = async (item: ConversationSummary) => {
    if (
      !window.confirm(
        t("一步到位：同时删除 BYOK 本地数据与 Cursor 侧的全部数据（侧边栏条目、气泡、快照、文件缓存）。请先完全退出 Cursor，否则 Cursor 会把删除的数据从内存写回。确定彻底删除？") +
          "\n" +
          labelOf(item),
      )
    ) {
      return;
    }
    setPending(item.conversation_id);
    try {
      const report: EverywhereRemoval = await api.deleteEverywhere(item.conversation_id);
      const lines: string[] = [];
      if (report.local) {
        lines.push(t("本地已删") + ` · ${localSummary(report.local)}`);
      } else {
        lines.push(t("本地无数据"));
      }
      if (report.cursor) {
        lines.push(cursorSummary(report.cursor));
      } else if (report.cursor_error) {
        lines.push(report.cursor_error);
      }
      showMessage(lines.join("\n"));
      refresh();
    } catch (error: unknown) {
      showMessage(error instanceof Error ? error.message : String(error));
    } finally {
      setPending(null);
    }
  };

  /** Removes every ticked conversation in a single request. */
  const removeSelection = async (cursor: boolean) => {
    const ids = selectedIds;
    if (!ids.length) return;
    const prompt = cursor
      ? t("将彻底删除选中的对话：BYOK 本地数据 + Cursor 侧全部数据（侧边栏条目、气泡、快照、文件缓存）。请先完全退出 Cursor，否则删除会被写回。确定继续？")
      : t("将删除选中对话的 BYOK 本地数据，Cursor 侧边栏条目保留。确定继续？");
    if (!window.confirm(`${prompt}\n${t("共")} ${ids.length} ${t("条对话")}`)) return;
    setBulkBusy(true);
    try {
      const report: SelectionRemoval = await api.deleteSelection(ids, cursor);
      const lines: string[] = [];
      if (report.local_conversations > 0) {
        lines.push(
          `${t("本地已删")} ${report.local_conversations} ${t("条对话")} · ${localSummary(report.local)}`,
        );
      } else {
        lines.push(t("本地无数据"));
      }
      if (cursor) {
        if (report.cursor) lines.push(cursorSummary(report.cursor));
        else if (report.cursor_error) lines.push(report.cursor_error);
      }
      if (report.failed.length) {
        lines.push(`${report.failed.length} ${t("条删除失败")}：${report.failed[0].error}`);
      }
      showMessage(lines.join("\n"));
      setSelected(new Set());
      refresh();
    } catch (error: unknown) {
      showMessage(error instanceof Error ? error.message : String(error));
    } finally {
      setBulkBusy(false);
    }
  };

  const content = (
    <div className={styles.page}>
      <div className={styles.header}>
        <span className={styles.count}>{items.length}</span>
        <span className={styles.hint}>
          {t("按项目分组")} · {t("彻底删除前请先完全退出 Cursor")}
        </span>
        <span className={styles.headerActions}>
          <button
            type="button"
            className={styles.selectAll}
            disabled={items.length === 0}
            onClick={() => toggleMany(items.map((item) => item.conversation_id), !allSelected)}
          >
            {allSelected ? t("取消全选") : t("全选")}
          </button>
          <span className={styles.selectedCount}>
            {t("已选")} {selected.size}
          </span>
          <button
            type="button"
            className={styles.bulkDelete}
            disabled={!selected.size || bulkBusy}
            onClick={() => void removeSelection(false)}
          >
            {t("删本地选中")}
          </button>
          <button
            type="button"
            className={styles.bulkEverywhere}
            disabled={!selected.size || bulkBusy}
            onClick={() => void removeSelection(true)}
          >
            {bulkBusy ? t("删除中") : t("彻底删除选中")}
          </button>
        </span>
      </div>
      <div className={styles.table}>
        <div className={styles.row + " " + styles.head}>
          <CheckBox
            checked={allSelected}
            indeterminate={selected.size > 0}
            disabled={items.length === 0}
            title={t("全选 / 取消全选")}
            onChange={(checked) =>
              toggleMany(items.map((item) => item.conversation_id), checked)
            }
          />
          <span>{t("对话")}</span>
          <span className={styles.num}>{t("消息")}</span>
          <span className={styles.num}>{t("快照")}</span>
          <span className={styles.time}>{t("最后使用")}</span>
          <span className={styles.action} />
        </div>
        {groups.map((group) => {
          const isCollapsed = Boolean(collapsed[group.key]);
          const ids = group.items.map((item) => item.conversation_id);
          const ticked = ids.filter((id) => selected.has(id)).length;
          return (
            <div className={styles.group} key={group.key}>
              <div className={styles.groupRow}>
                <CheckBox
                  checked={ticked === ids.length}
                  indeterminate={ticked > 0}
                  title={t("选中整个项目")}
                  onChange={(checked) => toggleMany(ids, checked)}
                />
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
                  <span className={styles.groupTime}>{timeOf(group.lastUsed)}</span>
                </button>
              </div>
              {!isCollapsed &&
                group.items.map((item) => (
                  <div
                    className={
                      styles.row +
                      " " +
                      styles.item +
                      (selected.has(item.conversation_id) ? " " + styles.itemSelected : "")
                    }
                    key={item.conversation_id}
                  >
                    <CheckBox
                      checked={selected.has(item.conversation_id)}
                      onChange={(checked) => toggleOne(item.conversation_id, checked)}
                    />
                    <span
                      className={styles.preview}
                      title={`${labelOf(item)}\n${item.conversation_id}`}
                    >
                      <span className={styles.title}>{labelOf(item)}</span>
                      <span className={styles.meta}>
                        {metaOf(item)}
                        {item.in_cursor && <span className={styles.badgeCursor}>Cursor</span>}
                        {item.local && <span className={styles.badgeLocal}>{t("本地")}</span>}
                        {!item.local && (
                          <span className={styles.badgeOnlyCursor}>{t("仅 Cursor 侧")}</span>
                        )}
                      </span>
                    </span>
                    <span className={styles.num}>{item.local ? item.message_count : "—"}</span>
                    <span className={styles.num}>{item.local ? item.checkpoint_count : "—"}</span>
                    <span className={styles.time}>{timeOf(item.last_used_ms)}</span>
                    <span className={styles.action}>
                      <button
                        type="button"
                        className={styles.sidebar}
                        disabled={pending === item.conversation_id || bulkBusy}
                        onClick={() => void removeSidebar(item)}
                      >
                        {t("清侧边栏")}
                      </button>
                      <button
                        type="button"
                        className={styles.everywhere}
                        disabled={pending === item.conversation_id || bulkBusy}
                        onClick={() => void removeEverywhere(item)}
                      >
                        {pending === item.conversation_id ? t("删除中") : t("彻底删除")}
                      </button>
                      <button
                        type="button"
                        className={styles.delete}
                        disabled={pending === item.conversation_id || bulkBusy || !item.local}
                        title={item.local ? undefined : t("BYOK 本地没有这条对话的数据")}
                        onClick={() => void removeLocal(item)}
                      >
                        {t("删本地")}
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
