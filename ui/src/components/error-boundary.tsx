import { Component } from "react";
import type { ErrorInfo, ReactNode } from "react";
import { AlertTriangle } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";

interface Props {
  children: ReactNode;
  fallback?: ReactNode;
}

interface State {
  hasError: boolean;
  error: Error | null;
  resetKey: number;
}

export class ErrorBoundary extends Component<Props, State> {
  constructor(props: Props) {
    super(props);
    this.state = { hasError: false, error: null, resetKey: 0 };
  }

  static getDerivedStateFromError(error: unknown): Partial<State> {
    const normalised =
      error instanceof Error ? error : new Error(String(error ?? "Unknown error"));
    return { hasError: true, error: normalised };
  }

  componentDidCatch(error: unknown, errorInfo: ErrorInfo) {
    console.error("ErrorBoundary caught:", error, errorInfo);
  }

  handleReset = () => {
    this.setState((prev) => ({
      hasError: false,
      error: null,
      resetKey: prev.resetKey + 1,
    }));
  };

  render() {
    if (this.state.hasError) {
      if (this.props.fallback) {
        return this.props.fallback;
      }

      return (
        <Card className="border-destructive/50">
          <CardContent className="flex flex-col items-center gap-3 py-8">
            <AlertTriangle className="h-8 w-8 text-destructive" />
            <p className="text-sm font-medium">Something went wrong</p>
            <p className="max-w-md text-center text-xs text-muted-foreground">
              {this.state.error?.message}
            </p>
            <Button variant="outline" size="sm" onClick={this.handleReset}>
              Try again
            </Button>
          </CardContent>
        </Card>
      );
    }

    return (
      <span key={this.state.resetKey} style={{ display: "contents" }}>
        {this.props.children}
      </span>
    );
  }
}
