/**
 * Group label between palette sections — `RECENTS`, `LOCALHOST · 2`,
 * `OFFLINE · 1`. 11px medium uppercase, wide tracking, muted.
 */

export interface SectionHeaderProps {
  label: string
  /** Optional trailing count or note, e.g. `2` or `OFFLINE`. Rendered
   * right after a thin middle dot, same color as the label. */
  count?: number | string
}

export function SectionHeader({ label, count }: SectionHeaderProps) {
  return (
    <div
      className="flex items-center px-3 pb-1.5 pt-3.5 font-medium"
      style={{
        color: 'var(--text-tertiary)',
        fontSize: 10,
        letterSpacing: '0.08em',
        textTransform: 'uppercase',
      }}
    >
      <span>{label}</span>
      {count !== undefined && <span className="ml-1.5">· {count}</span>}
    </div>
  )
}
