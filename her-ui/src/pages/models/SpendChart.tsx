import React, { useState } from 'react';
import { TokenSeriesBucket } from '../../api/config';
import { formatDateTimeMs } from '../../utils/format';

interface SpendChartProps {
  buckets: TokenSeriesBucket[];
  metric: 'cost' | 'tokens';
  symbol: string;
}

const SVG_WIDTH = 640;
const SVG_HEIGHT = 200;
const PAD_LEFT = 56;
const PAD_RIGHT = 12;
const PAD_TOP = 16;
const PAD_BOTTOM = 28;
const PLOT_WIDTH = SVG_WIDTH - PAD_LEFT - PAD_RIGHT;
const PLOT_HEIGHT = SVG_HEIGHT - PAD_TOP - PAD_BOTTOM;

function formatCompactAxis(value: number, metric: 'cost' | 'tokens', symbol: string): string {
  if (metric === 'cost') {
    return `${symbol}${value >= 1 ? value.toFixed(2) : value.toFixed(3)}`;
  }
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(1)}M`;
  if (value >= 1_000) return `${(value / 1_000).toFixed(1)}k`;
  return String(Math.round(value));
}

function formatShortTime(ms: number): string {
  try {
    return new Date(ms).toLocaleTimeString('zh-CN', {
      hour12: false,
      month: '2-digit',
      day: '2-digit',
      hour: '2-digit',
      minute: '2-digit',
    });
  } catch {
    return '';
  }
}

export const SpendChart: React.FC<SpendChartProps> = ({ buckets, metric, symbol }) => {
  const [activeIndex, setActiveIndex] = useState<number | null>(null);

  if (buckets.length === 0) {
    return (
      <div className="flex h-44 items-center justify-center rounded-md border border-dashed border-[var(--border-hairline)] text-xs text-[var(--text-muted)]">
        当前时间窗口暂无时间序列数据
      </div>
    );
  }

  const values = buckets.map((b) => (metric === 'cost' ? b.cost || 0 : b.tokens || 0));
  const maxVal = Math.max(...values, metric === 'cost' ? 0.0001 : 1);
  const stepWidth = PLOT_WIDTH / buckets.length;
  const barWidth = Math.max(2, stepWidth * 0.72);
  const barOffset = (stepWidth - barWidth) / 2;

  const selectedIdx =
    activeIndex !== null && activeIndex >= 0 && activeIndex < buckets.length
      ? activeIndex
      : buckets.length - 1;
  const selectedBucket = buckets[selectedIdx];

  return (
    <div className="space-y-2">
      <svg
        viewBox={`0 0 ${SVG_WIDTH} ${SVG_HEIGHT}`}
        role="img"
        aria-label={metric === 'cost' ? '花费时间序列柱状图' : 'Token 用量时间序列柱状图'}
        className="h-52 w-full select-none overflow-visible"
      >
        {[0, 0.5, 1].map((ratio) => {
          const y = PAD_TOP + PLOT_HEIGHT * (1 - ratio);
          return (
            <g key={ratio}>
              <line
                x1={PAD_LEFT}
                y1={y}
                x2={SVG_WIDTH - PAD_RIGHT}
                y2={y}
                className="stroke-[var(--border-hairline)]"
                strokeWidth={1}
                strokeDasharray={ratio === 0 ? undefined : '3 3'}
              />
              <text
                x={PAD_LEFT - 6}
                y={y + 3}
                textAnchor="end"
                className="fill-[var(--text-muted)] text-[10px]"
              >
                {formatCompactAxis(maxVal * ratio, metric, symbol)}
              </text>
            </g>
          );
        })}

        {buckets.map((bucket, idx) => {
          const val = metric === 'cost' ? bucket.cost || 0 : bucket.tokens || 0;
          const barHeight = val > 0 ? Math.max(2, (val / maxVal) * PLOT_HEIGHT) : 0;
          const x = PAD_LEFT + idx * stepWidth + barOffset;
          const y = PAD_TOP + PLOT_HEIGHT - barHeight;
          const isSelected = idx === selectedIdx;

          return (
            <g
              key={bucket.t}
              onMouseEnter={() => setActiveIndex(idx)}
              onClick={() => setActiveIndex(idx)}
              className="cursor-pointer"
            >
              <rect
                x={PAD_LEFT + idx * stepWidth}
                y={PAD_TOP}
                width={stepWidth}
                height={PLOT_HEIGHT}
                fill="transparent"
              />
              {barHeight > 0 && (
                <rect
                  x={x}
                  y={y}
                  width={barWidth}
                  height={barHeight}
                  rx={2}
                  className={
                    isSelected ? 'fill-[var(--accent)]' : 'fill-[var(--accent)]/65'
                  }
                />
              )}
            </g>
          );
        })}

        <text
          x={PAD_LEFT}
          y={SVG_HEIGHT - 8}
          textAnchor="start"
          className="fill-[var(--text-muted)] text-[10px]"
        >
          {formatShortTime(buckets[0].t)}
        </text>
        <text
          x={SVG_WIDTH - PAD_RIGHT}
          y={SVG_HEIGHT - 8}
          textAnchor="end"
          className="fill-[var(--text-muted)] text-[10px]"
        >
          {formatShortTime(buckets[buckets.length - 1].t)}
        </text>
      </svg>

      {selectedBucket && (
        <div className="flex flex-wrap items-center justify-between gap-2 rounded-md border border-[var(--border-hairline)] bg-[var(--bg-subtle)]/40 px-3 py-1.5 text-[11.5px] text-[var(--text-secondary)]">
          <span className="font-mono text-[var(--text-primary)]">
            {formatDateTimeMs(selectedBucket.t)}
          </span>
          <span>
            花费:{' '}
            <strong className="text-[var(--text-primary)]">
              {symbol}
              {(selectedBucket.cost || 0).toFixed(4)}
            </strong>{' '}
            · Tokens:{' '}
            <strong className="text-[var(--text-primary)]">
              {(selectedBucket.tokens || 0).toLocaleString()}
            </strong>{' '}
            · 回合 {selectedBucket.turns ?? 0} / 步 {selectedBucket.steps ?? 0}
            {selectedBucket.tier ? ` · 费率档: ${selectedBucket.tier}` : ''}
          </span>
        </div>
      )}
    </div>
  );
};
