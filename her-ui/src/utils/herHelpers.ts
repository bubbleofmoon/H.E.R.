import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { SettingItemView } from '../api/client';
import { useConsole } from '../stores/consoleStore';
import { formatErrorMessage } from './format';

export const INPUT_CLASS =
  'w-full rounded-md border border-[var(--border-strong)] bg-[var(--bg-surface)] px-2.5 py-1.5 text-xs text-[var(--text-primary)] placeholder:text-[var(--text-muted)] focus:border-[var(--accent)] focus:outline-none disabled:opacity-50';

export function utf16Length(value: string): number {
  return value.length;
}

export function unicodeScalarLength(value: string): number {
  return Array.from(value).length;
}

export function utf8ByteLength(value: string): number {
  return new TextEncoder().encode(value).length;
}

function normalizeForSettingCompare(value: unknown): unknown {
  if (value === undefined || value === null || value === '') {
    return null;
  }
  if (Array.isArray(value)) {
    return value.map(normalizeForSettingCompare);
  }
  if (typeof value === 'object') {
    const obj = value as Record<string, unknown>;
    const sorted: Record<string, unknown> = {};
    for (const key of Object.keys(obj).sort()) {
      sorted[key] = normalizeForSettingCompare(obj[key]);
    }
    return sorted;
  }
  return value;
}

export function settingValuesEqual(a: unknown, b: unknown): boolean {
  return (
    JSON.stringify(normalizeForSettingCompare(a)) ===
    JSON.stringify(normalizeForSettingCompare(b))
  );
}

export function useSettingsDrafts(
  items: SettingItemView[],
  labelPrefix: string,
  options: { manageDirtyBar?: boolean } = {},
) {
  const { saveSetting, resetSetting, openConfirm, pushToast, setDirtyEntry } = useConsole();
  const [drafts, setDrafts] = useState<Record<string, unknown>>({});
  const mountedRef = useRef(true);
  const savingRef = useRef(false);
  const draftRevRef = useRef<Record<string, number>>({});
  const inFlightKeysRef = useRef<Set<string>>(new Set());
  const manageDirtyBar = options.manageDirtyBar !== false;

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      if (manageDirtyBar) {
        setDirtyEntry(null);
      }
    };
  }, [manageDirtyBar, setDirtyEntry]);

  const dirtyKeys = useMemo(
    () =>
      Object.entries(drafts)
        .filter(([key, val]) => {
          const item = items.find((s) => s.key === key);
          return item ? !settingValuesEqual(val, item.value) : false;
        })
        .map(([key]) => key),
    [drafts, items],
  );

  const setDraft = useCallback((key: string, value: unknown) => {
    draftRevRef.current[key] = (draftRevRef.current[key] ?? 0) + 1;
    setDrafts((prev) => ({ ...prev, [key]: value }));
  }, []);

  const discardAll = useCallback(() => {
    for (const key of Object.keys(draftRevRef.current)) {
      draftRevRef.current[key] = (draftRevRef.current[key] ?? 0) + 1;
    }
    setDrafts({});
  }, []);

  const saveAll = useCallback(async (): Promise<{ saved: number; failed: string[] }> => {
    if (savingRef.current || dirtyKeys.length === 0) {
      return { saved: 0, failed: [] };
    }
    savingRef.current = true;
    const snapshots = dirtyKeys.map((key) => ({
      key,
      value: drafts[key],
      rev: draftRevRef.current[key] ?? 0,
    }));
    const failed: string[] = [];
    let saved = 0;

    try {
      for (const { key, value, rev } of snapshots) {
        if ((draftRevRef.current[key] ?? 0) !== rev || inFlightKeysRef.current.has(key)) {
          continue;
        }
        inFlightKeysRef.current.add(key);
        try {
          await saveSetting(key, value, false);
          saved += 1;
          if (!mountedRef.current) return { saved, failed };
          setDrafts((prev) => {
            if (!(key in prev) || (draftRevRef.current[key] ?? 0) !== rev) {
              return prev;
            }
            const next = { ...prev };
            delete next[key];
            return next;
          });
        } catch (err) {
          failed.push(`${key}: ${formatErrorMessage(err, '保存失败')}`);
        } finally {
          inFlightKeysRef.current.delete(key);
        }
      }
      if (mountedRef.current) {
        if (failed.length > 0) {
          pushToast(`部分${labelPrefix}保存失败：${failed[0]}`, 'danger');
        } else if (saved > 0) {
          pushToast(`已保存 ${saved} 项${labelPrefix}`, 'success');
        }
      }
    } finally {
      savingRef.current = false;
    }
    return { saved, failed };
  }, [dirtyKeys, drafts, saveSetting, pushToast, labelPrefix]);

  useEffect(() => {
    if (!manageDirtyBar) return;
    if (dirtyKeys.length > 0) {
      setDirtyEntry({
        count: dirtyKeys.length,
        label: `${labelPrefix}有 ${dirtyKeys.length} 项未保存更改`,
        onSave: async () => {
          await saveAll();
        },
        onDiscard: discardAll,
      });
    } else {
      setDirtyEntry(null);
    }
  }, [manageDirtyBar, dirtyKeys.length, labelPrefix, setDirtyEntry, saveAll, discardAll]);

  const handleReset = useCallback(
    (key: string) => {
      openConfirm({
        title: `确认恢复 ${key} 为默认值？`,
        description: '恢复后将清除该设置项的自定义值，并同步移除未保存草稿。',
        confirmLabel: '确认恢复',
        onConfirm: async () => {
          if (inFlightKeysRef.current.has(key) || savingRef.current) {
            pushToast(`${key} 正在保存中，请稍后再恢复默认`, 'warn');
            return;
          }
          const rev = draftRevRef.current[key] ?? 0;
          inFlightKeysRef.current.add(key);
          try {
            await resetSetting(key, false);
            if (!mountedRef.current) return;
            setDrafts((prev) => {
              if (!(key in prev) || (draftRevRef.current[key] ?? 0) !== rev) {
                return prev;
              }
              const next = { ...prev };
              delete next[key];
              return next;
            });
          } catch (err) {
            if (mountedRef.current) {
              pushToast(formatErrorMessage(err, `恢复 ${key} 默认值失败`), 'danger');
            }
          } finally {
            inFlightKeysRef.current.delete(key);
          }
        },
      });
    },
    [openConfirm, resetSetting, pushToast],
  );

  return {
    drafts,
    dirtyKeys,
    setDraft,
    discardAll,
    saveAll,
    handleReset,
  };
}
