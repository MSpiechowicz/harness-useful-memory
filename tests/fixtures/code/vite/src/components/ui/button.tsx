import { cn } from '@/lib/utils';

export function Button(props: { className?: string; children?: string }) {
  return <button className={cn('btn', props.className ?? '')}>{props.children}</button>;
}
