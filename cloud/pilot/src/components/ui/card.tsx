// shadcn/ui-style Card.
import * as React from 'react';
import { cn } from '@/lib/utils';

export function Card({ className, ...props }: React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      className={cn('rounded-3xl border border-border bg-card/70 text-card-foreground shadow-xl backdrop-blur', className)}
      {...props}
    />
  );
}
