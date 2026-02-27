import type { ReactNode } from "react";

interface InfoItem {
  label: string;
  value: ReactNode;
}

interface InfoGridProps {
  items: InfoItem[];
  columns?: 4 | 5 | 6;
}

const colsClass: Record<4 | 5 | 6, string> = {
  4: "lg:grid-cols-4",
  5: "lg:grid-cols-5",
  6: "lg:grid-cols-6",
};

export function InfoGrid({ items, columns = 6 }: InfoGridProps) {
  return (
    <div className={`grid gap-4 sm:grid-cols-2 ${colsClass[columns]}`}>
      {items.map((item) => (
        <div key={item.label} className="rounded-lg border px-4 py-3">
          <p className="text-xs text-muted-foreground">{item.label}</p>
          <div className="mt-0.5 text-sm font-medium">{item.value}</div>
        </div>
      ))}
    </div>
  );
}
