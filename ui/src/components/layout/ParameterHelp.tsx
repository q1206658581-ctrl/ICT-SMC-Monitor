import type { ReactElement, LabelHTMLAttributes } from 'react';
import * as Tooltip from '@radix-ui/react-tooltip';

export function Help({ text, children }: { text: string; children: ReactElement }) {
  return <Tooltip.Provider delayDuration={250}><Tooltip.Root>
    <Tooltip.Trigger asChild>{children}</Tooltip.Trigger>
    <Tooltip.Portal><Tooltip.Content side="left" sideOffset={8} collisionPadding={12}
      className="z-[100] max-w-[340px] rounded-md border border-border bg-bg-3 px-3 py-2 text-sm text-text-1 shadow-lg leading-relaxed">
      {text}<Tooltip.Arrow className="fill-bg-3" />
    </Tooltip.Content></Tooltip.Portal>
  </Tooltip.Root></Tooltip.Provider>;
}

export function Parameter({ title, children, ...props }: LabelHTMLAttributes<HTMLLabelElement>) {
  return <Help text={title || '开启显示此项，关闭隐藏此项；不改变后台检测。'}>
    <label {...props} tabIndex={0}>{children}</label>
  </Help>;
}
