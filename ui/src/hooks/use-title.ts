import { useEffect } from "react";

export function useTitle(page?: string) {
  useEffect(() => {
    document.title = page ? `${page} - Strøm` : "Strøm";
  }, [page]);
}
