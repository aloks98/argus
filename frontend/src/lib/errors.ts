// The fetch layer's errors are terse by design (`fleet request failed: 503`,
// a bare `TypeError: Failed to fetch` when the network is down) — fine for a
// console, not for an Alert. This maps the common shapes to a plain sentence
// with a next step, keeping the raw message as a parenthetical so nothing is
// lost. Anything unrecognized (e.g. a server-side validation message from
// mint) passes through untouched — those are already written for humans.

export function describeError(err: unknown): string {
  // fetch() rejects with TypeError when no HTTP exchange happened at all —
  // server down, DNS, or no network. There is no status code to parse.
  if (err instanceof TypeError) {
    return "Can't reach the control plane — check that the server is running and reachable from this browser.";
  }

  const message = err instanceof Error ? err.message : String(err);
  const status = /(?:^|[ :])([45]\d{2})$/.exec(message.trim())?.[1];

  if (status !== undefined && status.startsWith("5")) {
    return `The control plane hit an internal error. Try again — if it keeps failing, check the server logs. (${message})`;
  }
  if (status === "403") {
    return `You don't have permission for this action. (${message})`;
  }
  if (status === "404") {
    return `The control plane doesn't know this resource — it may have been removed. Refresh the page. (${message})`;
  }
  return message;
}
