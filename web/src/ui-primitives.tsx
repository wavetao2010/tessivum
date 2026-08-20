import type { IconProps } from '@deepseek-ai/dsh-client-ui-primitives';

export * from '@tessivum/ui-primitives-original';

function Mark({ size, className }: Required<Pick<IconProps, 'size'>> & Pick<IconProps, 'className'>) {
  return (
    <svg aria-hidden="true" className={className} fill="none" height={size} viewBox="0 0 24 24" width={size}>
      <path d="M3 3h8v8H3zM13 3h8v8h-8zM3 13h8v8H3zM13 13h8v8h-8z" fill="currentColor" opacity=".18" />
      <path d="M5 5h14v4H5zM10 9h4v10h-4z" fill="currentColor" />
    </svg>
  );
}

export function FishLogo({ size = 24, className }: IconProps) {
  return <Mark className={className} size={size} />;
}

export function BrandWordmark({ size = 24, className }: IconProps) {
  return (
    <svg aria-hidden="true" className={className} fill="none" height={size} viewBox="0 0 142 24" width={(size * 142) / 24}>
      <g><path d="M3 3h18v18H3z" fill="currentColor" opacity=".18" /><path d="M5 5h14v4H5zM10 9h4v10h-4z" fill="currentColor" /></g>
      <text fill="currentColor" fontFamily="var(--dsw-font-family)" fontSize="18" fontWeight="600" letterSpacing="-.35" x="30" y="18">tessivum</text>
    </svg>
  );
}
