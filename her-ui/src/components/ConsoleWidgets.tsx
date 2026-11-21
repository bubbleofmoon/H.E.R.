import React from 'react';
import { Search } from 'lucide-react';

export const EmptyState: React.FC<{
  icon?: React.ReactNode;
  title: string;
  description?: string;
  action?: React.ReactNode;
}> = ({ icon, title, description, action }) => (
  <div className="flex flex-col items-center justify-center rounded-md border border-dashed border-[var(--border-hairline)] bg-[var(--bg-subtle)]/40 px-4 py-6 text-center">
    {icon && <div className="mb-2 text-[var(--text-muted)]">{icon}</div>}
    <div className="text-xs font-medium text-[var(--text-primary)]">{title}</div>
    {description && (
      <p className="mt-1 max-w-md text-[11px] leading-relaxed text-[var(--text-muted)]">
        {description}
      </p>
    )}
    {action && <div className="mt-3">{action}</div>}
  </div>
);

export const KeyValueRow: React.FC<{
  label: string;
  value: React.ReactNode;
}> = ({ label, value }) => (
  <div className="flex items-start justify-between gap-3 border-b border-[var(--border-hairline)] py-1.5 text-xs last:border-b-0">
    <span className="shrink-0 text-[var(--text-muted)]">{label}</span>
    <span className="min-w-0 break-all text-right text-[var(--text-primary)]">{value}</span>
  </div>
);

export const SearchInput: React.FC<{
  value: string;
  onChange: (next: string) => void;
  placeholder?: string;
}> = ({ value, onChange, placeholder = '搜索…' }) => (
  <div className="relative">
    <Search className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-[var(--text-muted)]" />
    <input
      type="search"
      value={value}
      onChange={(e) => onChange(e.target.value)}
      placeholder={placeholder}
      className="h-8 w-full rounded-md border border-[var(--border-strong)] bg-[var(--bg-surface)] pl-8 pr-2.5 text-xs text-[var(--text-primary)] placeholder:text-[var(--text-muted)] focus:border-[var(--accent)] focus:outline-none"
    />
  </div>
);

export function SegmentedControl<T extends string>({
  value,
  onChange,
  options,
}: {
  value: T;
  onChange: (next: T) => void;
  options: { value: T; label: string }[];
}): React.ReactElement {
  return (
    <div className="inline-flex rounded-md border border-[var(--border-strong)] bg-[var(--bg-subtle)] p-0.5 text-xs">
      {options.map((opt) => {
        const active = opt.value === value;
        return (
          <button
            key={opt.value}
            type="button"
            onClick={() => onChange(opt.value)}
            className={`rounded px-2.5 py-1 font-medium transition-colors ${
              active
                ? 'bg-[var(--bg-surface)] text-[var(--text-primary)] shadow-xs'
                : 'text-[var(--text-secondary)] hover:text-[var(--text-primary)]'
            }`}
          >
            {opt.label}
          </button>
        );
      })}
    </div>
  );
}
