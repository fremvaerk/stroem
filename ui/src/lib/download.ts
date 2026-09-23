/** Save `blob` as `filename` through a temporary object URL. */
export function saveBlob(blob: Blob, filename: string): void {
  const url = URL.createObjectURL(blob);
  try {
    const a = document.createElement("a");
    a.href = url;
    a.download = filename;
    // Some browsers need the anchor in the DOM before .click().
    document.body.appendChild(a);
    a.click();
    a.remove();
  } finally {
    // Give the browser time to start the download before revoking.
    setTimeout(() => URL.revokeObjectURL(url), 60_000);
  }
}
