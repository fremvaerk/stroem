export function LoadingSpinner({ className }: { className?: string }) {
  return (
    <div className={className ?? "flex items-center justify-center py-20"}>
      <div className="h-8 w-8 animate-spin rounded-full border-4 border-muted border-t-primary" />
    </div>
  );
}
