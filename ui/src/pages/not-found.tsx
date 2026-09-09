import { Link, useLocation } from "react-router";
import { Button } from "@/components/ui/button";
import { useTitle } from "@/hooks/use-title";

export function NotFoundPage() {
  useTitle("Not found");
  const { pathname } = useLocation();
  return (
    <div className="py-20 text-center">
      <h1 className="text-2xl font-semibold tracking-tight">Page not found</h1>
      <p className="mt-2 text-sm text-muted-foreground">
        There is nothing at <code className="text-xs">{pathname}</code>.
      </p>
      <Button variant="link" asChild className="mt-2">
        <Link to="/">Back to dashboard</Link>
      </Button>
    </div>
  );
}
