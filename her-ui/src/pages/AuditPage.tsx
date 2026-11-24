import React, { useCallback, useEffect, useRef, useState } from 'react';
import { RefreshCw } from 'lucide-react';
import {
  AuditEntry,
  InboundGapsResponse,
  RecoveryRecord,
  TurnsResponse,
  fetchAuditRecent,
  fetchGlobalInboundGaps,
  fetchGlobalTurns,
  fetchRecoveryRecords,
} from '../api/security';
import { AUDIT_UPDATED_EVENT } from '../components/AuditInspector';
import { Button, PageHeader, Tabs } from '../components/Primitives';
import { useConsole } from '../stores/consoleStore';
import { formatErrorMessage } from '../utils/format';
import { AuditLogTab } from './audit/AuditLogTab';
import { RecoveryTab } from './audit/RecoveryTab';

const AUDIT_TABS = [
  { id: 'audit-log', label: '审计日志' },
  { id: 'recovery', label: '恢复与中断记录' },
];

export const AuditPage: React.FC = () => {
  const { activeTabs, setActiveTab, inspector, setInspector, pushToast } = useConsole();

  const currentTab = activeTabs['audit'] === 'recovery' ? 'recovery' : 'audit-log';

  const [auditEntries, setAuditEntries] = useState<AuditEntry[]>([]);
  const [auditError, setAuditError] = useState<string | null>(null);
  const [auditLimit, setAuditLimit] = useState(50);
  const [auditLoading, setAuditLoading] = useState(true);

  const [turnsData, setTurnsData] = useState<TurnsResponse | null>(null);
  const [turnsError, setTurnsError] = useState<string | null>(null);
  const [gapsData, setGapsData] = useState<InboundGapsResponse | null>(null);
  const [gapsError, setGapsError] = useState<string | null>(null);
  const [recoveryRows, setRecoveryRows] = useState<RecoveryRecord[]>([]);
  const [recoveryError, setRecoveryError] = useState<string | null>(null);
  const [nextBefore, setNextBefore] = useState<number | null>(null);
  const [componentFilter, setComponentFilter] = useState('');
  const [outcomeFilter, setOutcomeFilter] = useState('');
  const [recoveryLoading, setRecoveryLoading] = useState(false);
  const [loadingMoreRec, setLoadingMoreRec] = useState(false);

  const mountedRef = useRef(true);
  const auditGenRef = useRef(0);
  const recGenRef = useRef(0);
  const activeAuditLoadsRef = useRef(0);
  const activeRecLoadsRef = useRef(0);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const loadAudits = useCallback(async (limit = auditLimit) => {
    const gen = ++auditGenRef.current;
    activeAuditLoadsRef.current += 1;
    setAuditLoading(true);
    try {
      const res = await fetchAuditRecent(limit);
      if (!mountedRef.current || gen !== auditGenRef.current) return;
      setAuditEntries(res.entries);
      setAuditError(null);
    } catch (err) {
      if (!mountedRef.current || gen !== auditGenRef.current) return;
      setAuditError(formatErrorMessage(err, '加载审计日志失败'));
    } finally {
      activeAuditLoadsRef.current = Math.max(0, activeAuditLoadsRef.current - 1);
      if (mountedRef.current) {
        setAuditLoading(activeAuditLoadsRef.current > 0);
      }
    }
  }, [auditLimit]);

  const loadRecoveryTab = useCallback(
    async (comp = componentFilter, out = outcomeFilter) => {
      setLoadingMoreRec(false);
      const gen = ++recGenRef.current;
      activeRecLoadsRef.current += 1;
      setRecoveryLoading(true);
      setTurnsError(null);
      setGapsError(null);
      setRecoveryError(null);
      try {
        const [turnsRes, gapsRes, recRes] = await Promise.allSettled([
          fetchGlobalTurns(),
          fetchGlobalInboundGaps(),
          fetchRecoveryRecords({
            limit: 50,
            component: comp || undefined,
            outcome: out || undefined,
          }),
        ]);
        if (!mountedRef.current || gen !== recGenRef.current) return;

        if (turnsRes.status === 'fulfilled') {
          setTurnsData(turnsRes.value);
          setTurnsError(null);
        } else {
          setTurnsError(formatErrorMessage(turnsRes.reason, '读取回合账本失败'));
        }

        if (gapsRes.status === 'fulfilled') {
          setGapsData(gapsRes.value);
          setGapsError(null);
        } else {
          setGapsError(formatErrorMessage(gapsRes.reason, '读取入站缺口失败'));
        }

        if (recRes.status === 'fulfilled') {
          setRecoveryRows(recRes.value.records);
          setNextBefore(recRes.value.nextBefore);
          setRecoveryError(null);
        } else {
          setRecoveryError(formatErrorMessage(recRes.reason, '读取恢复运行记录失败'));
        }
      } finally {
        activeRecLoadsRef.current = Math.max(0, activeRecLoadsRef.current - 1);
        if (mountedRef.current) {
          setRecoveryLoading(activeRecLoadsRef.current > 0);
        }
      }
    },
    [componentFilter, outcomeFilter],
  );

  useEffect(() => {
    if (currentTab === 'audit-log') {
      void loadAudits(auditLimit);
    } else {
      void loadRecoveryTab(componentFilter, outcomeFilter);
    }
  }, [auditLimit, componentFilter, currentTab, loadAudits, loadRecoveryTab, outcomeFilter]);

  useEffect(() => {
    const onUpdated = () => {
      if (currentTab === 'audit-log') {
        void loadAudits(auditLimit);
      }
    };
    window.addEventListener(AUDIT_UPDATED_EVENT, onUpdated);
    return () => window.removeEventListener(AUDIT_UPDATED_EVENT, onUpdated);
  }, [auditLimit, currentTab, loadAudits]);

  const loadMoreRecovery = async () => {
    if (nextBefore === null || loadingMoreRec || recoveryLoading) return;
    const gen = recGenRef.current;
    setLoadingMoreRec(true);
    try {
      const res = await fetchRecoveryRecords({
        limit: 50,
        before: nextBefore,
        component: componentFilter || undefined,
        outcome: outcomeFilter || undefined,
      });
      if (!mountedRef.current || gen !== recGenRef.current) return;
      setRecoveryRows((prev) => {
        const seen = new Set(prev.map((r) => r.seq));
        const appended = res.records.filter((r) => !seen.has(r.seq));
        return [...prev, ...appended];
      });
      setNextBefore(res.nextBefore);
    } catch (err) {
      if (mountedRef.current && gen === recGenRef.current) {
        pushToast(formatErrorMessage(err, '加载更早恢复记录失败'), 'danger');
      }
    } finally {
      if (mountedRef.current && gen === recGenRef.current) {
        setLoadingMoreRec(false);
      }
    }
  };

  return (
    <div className="space-y-5">
      <PageHeader
        title="审计与恢复"
        description="追踪敏感配置变更快照与文件校验摘要、执行受控撤销，并排查全局暂停回合、入站缺口与依赖恢复流水。"
        actions={
          <Button
            size="sm"
            onClick={() =>
              void (currentTab === 'audit-log'
                ? loadAudits(auditLimit)
                : loadRecoveryTab(componentFilter, outcomeFilter))
            }
            disabled={currentTab === 'audit-log' ? auditLoading : recoveryLoading}
          >
            <RefreshCw
              className={`h-3.5 w-3.5 ${(currentTab === 'audit-log' ? auditLoading : recoveryLoading) ? 'animate-spin' : ''}`}
            />
            刷新视图
          </Button>
        }
      />

      <Tabs
        tabs={AUDIT_TABS}
        active={currentTab}
        onChange={(tab) => setActiveTab('audit', tab)}
      />

      {currentTab === 'audit-log' ? (
        <AuditLogTab
          entries={auditEntries}
          error={auditError}
          limit={auditLimit}
          loading={auditLoading}
          inspector={inspector}
          onInspect={(id) => setInspector({ kind: 'audit', id })}
          onRetry={() => void loadAudits(auditLimit)}
          onLoadMore={() => setAuditLimit((prev) => Math.min(200, prev + 50))}
        />
      ) : (
        <RecoveryTab
          turnsData={turnsData}
          turnsError={turnsError}
          gapsData={gapsData}
          gapsError={gapsError}
          recoveryRows={recoveryRows}
          recoveryError={recoveryError}
          nextBefore={nextBefore}
          componentFilter={componentFilter}
          setComponentFilter={setComponentFilter}
          outcomeFilter={outcomeFilter}
          setOutcomeFilter={setOutcomeFilter}
          loading={recoveryLoading}
          loadingMore={loadingMoreRec}
          onRefresh={() => loadRecoveryTab(componentFilter, outcomeFilter)}
          onLoadMore={loadMoreRecovery}
        />
      )}
    </div>
  );
};
