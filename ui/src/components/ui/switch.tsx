import { useId } from 'react';

type SwitchProps = {
  checked: boolean;
  onCheckedChange: (b: boolean) => void;
  ariaLabel?: string;
  disabled?: boolean;
};

export function Switch({ checked, onCheckedChange, ariaLabel, disabled }: SwitchProps) {
  const id = useId();
  return (
    <button
      id={id}
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={ariaLabel}
      disabled={disabled}
      onClick={() => { if (!disabled) onCheckedChange(!checked); }}
      style={{
        width: 28, height: 16, borderRadius: 8,
        background: disabled ? 'var(--bg-3)' : (checked ? 'var(--accent)' : 'var(--bg-3)'),
        border: '1px solid var(--border)',
        position: 'relative', cursor: disabled ? 'not-allowed' : 'pointer',
        transition: 'background 120ms',
        opacity: disabled ? 0.4 : 1,
      }}
    >
      <span
        style={{
          position: 'absolute', top: 1, left: checked ? 13 : 1,
          width: 12, height: 12, borderRadius: 6,
          background: 'var(--text-1)', transition: 'left 120ms',
          display: 'block',
        }}
      />
    </button>
  );
}
