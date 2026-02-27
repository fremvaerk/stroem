import { Button } from "@/components/ui/button";

interface PaginationControlsProps {
  offset: number;
  pageSize: number;
  /** Whether there might be more items (typically: items.length >= pageSize) */
  hasMore: boolean;
  onPrevious: () => void;
  onNext: () => void;
}

export function PaginationControls({
  offset,
  pageSize,
  hasMore,
  onPrevious,
  onNext,
}: PaginationControlsProps) {
  return (
    <div className="mt-4 flex items-center justify-between">
      <Button
        variant="outline"
        size="sm"
        disabled={offset === 0}
        onClick={onPrevious}
      >
        Previous
      </Button>
      <span className="text-xs text-muted-foreground">
        Page {Math.floor(offset / pageSize) + 1}
      </span>
      <Button
        variant="outline"
        size="sm"
        disabled={!hasMore}
        onClick={onNext}
      >
        Next
      </Button>
    </div>
  );
}
