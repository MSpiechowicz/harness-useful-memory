import { cn } from '@/lib/utils';
import { formatPrice } from '@/lib/format';

export function Card(props: { price: number }) {
  return <div className={cn('card')}>{formatPrice(props.price)}</div>;
}
