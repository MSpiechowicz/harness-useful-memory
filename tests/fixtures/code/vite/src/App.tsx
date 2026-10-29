import { useState } from 'react';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { useToast } from '@/hooks/use-toast';

export default function App() {
  const [count] = useState(0);
  const { toast } = useToast();

  return (
    <main>
      <Card price={count} />
      <Button>{toast('hi')}</Button>
    </main>
  );
}
