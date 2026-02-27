import { Button } from "@/components/ui/button";

interface PaginationControlsProps {
  offset: number;
  pageSize: number;
  total: number;
  onPrevious: () => void;
  onNext: () => void;
}

export function PaginationControls({
  offset,
  pageSize,
  total,
  onPrevious,
  onNext,
}: PaginationControlsProps) {
  const currentPage = Math.floor(offset / pageSize) + 1;
  const totalPages = Math.max(1, Math.ceil(total / pageSize));

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
        Page {currentPage} of {totalPages}
        <span className="ml-2">({total} {total === 1 ? "item" : "items"})</span>
      </span>
      <Button
        variant="outline"
        size="sm"
        disabled={currentPage >= totalPages}
        onClick={onNext}
      >
        Next
      </Button>
    </div>
  );
}
