// Assertion messages must stay small: some fixtures hold tens of megabytes of state, and a single
// multi-megabyte output line stalls the GitHub Actions runner's log processing for hours (#13).
const MAX_STRING = 200;
const MAX_TOTAL = 8000;

/** JSON for a failure message with long strings and the overall text clipped. */
export function stateSummary(value: unknown): string {
  const text = JSON.stringify(value, (_key, item: unknown) =>
    typeof item === 'string' && item.length > MAX_STRING ? `${item.slice(0, MAX_STRING)}…[${item.length} chars]` : item);
  return text.length > MAX_TOTAL ? `${text.slice(0, MAX_TOTAL)}…[${text.length} chars]` : text;
}
