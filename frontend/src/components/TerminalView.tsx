// Interactive shell over a WebSocket. Binary frames carry PTY bytes both
// ways; Text frames are asymmetric — browser->server they're the JSON
// resize control, but the only server->browser Text frame is a
// human-readable close reason (e.g. "agent not connected") sent when no PTY
// was ever opened, since there's no PtyOutput to carry that failure.
//
// Maximize is a LAYOUT toggle on the same mounted terminal, never a
// remount (which would drop the shell). No auto-reconnect: a closed shell
// is gone, so we surface "Start new session" rather than silently opening
// a fresh one behind the same view.
import { useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { Button } from "@e412/rnui-react";
import { terminalWsUrl } from "../api";

export default function TerminalView({ machineId }: { machineId: string }) {
  const hostRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const [maximized, setMaximized] = useState(false);
  const [closed, setClosed] = useState<string | null>(null);
  // Whether the operator dismissed the "session ended" overlay to read the
  // dead session's scrollback underneath it.
  const [dismissed, setDismissed] = useState(false);
  const [gen, setGen] = useState(0); // bump to start a new session

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    setClosed(null);
    setDismissed(false);

    const term = new Terminal({ convertEol: false, fontFamily: "monospace", fontSize: 13 });
    termRef.current = term;
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host);
    fit.fit();
    // Focus immediately: without this the operator has to click into the
    // terminal before the first keystroke lands anywhere, which is not
    // obvious for something that looks like a live shell prompt.
    term.focus();

    const ws = new WebSocket(terminalWsUrl(machineId));
    ws.binaryType = "arraybuffer";
    const enc = new TextEncoder();

    // The most specific close reason seen so far. onmessage/onerror can
    // both fire before the close event that follows them, and neither must
    // be clobbered by onclose's generic fallback — whichever handler runs
    // first records its reason; later handlers only fill in if still unset.
    let reason: string | null = null;

    const sendResize = () => {
      fit.fit();
      if (ws.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify({ resize: { cols: term.cols, rows: term.rows } }));
      }
    };

    ws.onopen = () => sendResize();
    ws.onmessage = (e) => {
      if (typeof e.data === "string") {
        // The only Text frame the server ever sends browser-ward: a
        // human-readable reason, delivered immediately before it closes the
        // socket. Surface it right away and remember it so the close event
        // that follows doesn't overwrite it with "session closed".
        reason = e.data;
        setClosed(reason);
        return;
      }
      term.write(new Uint8Array(e.data as ArrayBuffer));
    };
    ws.onclose = (e) => {
      // A normal, reasonless close (shell exited, idle timeout) keeps the
      // generic "session closed" wording; an abnormal close's `reason` wins
      // over that. Either way, a reason already recorded by onmessage/
      // onerror wins over both — it's the most specific thing heard.
      if (reason === null) {
        reason =
          e.reason || (e.code === 1000 ? "session closed" : `connection closed (code ${e.code})`);
      }
      setClosed(reason);
    };
    ws.onerror = () => {
      // Per the WebSocket spec a fatal error is always followed by a close
      // event, which would otherwise overwrite this with the generic
      // message — record it now so onclose's guard above preserves it.
      if (reason === null) reason = "connection error";
      setClosed(reason);
    };

    const dataSub = term.onData((s) => {
      if (ws.readyState === WebSocket.OPEN) ws.send(enc.encode(s));
    });

    // Refit on container resize (covers maximize/restore too).
    const ro = new ResizeObserver(() => sendResize());
    ro.observe(host);

    return () => {
      dataSub.dispose();
      ro.disconnect();
      // Detach handlers before closing: browser close events are async, so
      // a stale onclose from THIS (dead) socket could otherwise fire after
      // "Start new session" bumped `gen` and opened a live one — clobbering
      // the new session's banner. `ws.close()` still runs; only callbacks
      // are removed.
      ws.onopen = null;
      ws.onmessage = null;
      ws.onclose = null;
      ws.onerror = null;
      ws.close();
      termRef.current = null;
      term.dispose();
    };
  }, [machineId, gen]);

  // Restore keyboard focus after a maximize/restore toggle: the wrapper's
  // layout class changes, but the same mounted `Terminal` stays put, so a
  // plain `.focus()` is enough — no remount to race against. (Harmlessly
  // redundant with the mount-time `term.focus()` above.)
  useEffect(() => {
    termRef.current?.focus();
  }, [maximized]);

  return (
    <div
      className={
        maximized ? "fixed inset-0 z-50 flex flex-col bg-background p-2" : "flex h-[70vh] flex-col"
      }
    >
      <div className="flex items-center justify-between pb-1">
        <span className="font-mono text-[10px] uppercase tracking-widest text-muted-foreground">
          {closed ?? "interactive shell"}
        </span>
        <div className="flex gap-2">
          {/* Only once the overlay is gone: while it is up it carries its own
              copy of this button, and two identical "Start new session"
              buttons on screen at once is just noise. */}
          {closed && dismissed && (
            <Button size="sm" variant="outline" onClick={() => setGen((g) => g + 1)}>
              Start new session
            </Button>
          )}
          <Button size="sm" variant="outline" onClick={() => setMaximized((m) => !m)}>
            {maximized ? "Restore" : "Maximize"}
          </Button>
        </div>
      </div>
      {/* Border is load-bearing, not decoration: the terminal is always
          `bg-black`, and dark mode's page background is #000000 too — no
          edge means the terminal is invisible against the page. `--border`
          resolves differently per theme, so one border class works for both;
          `bg-black` and padding live here too so the inset gap reads as
          terminal, not page showing through.

          Both border AND padding MUST live on THIS wrapper, one level above
          the xterm mount div, not on the mount div itself: `FitAddon.
          proposeDimensions()` measures the mount div's parent via
          `getComputedStyle(...).height/.width`, which — under this app's
          `border-box` reset — INCLUDES the parent's border but not xterm's
          own padding. A border on the mount div would nudge cols/rows up a
          fraction of a cell and clip the last row/column. Keeping the mount
          div itself borderless (h-full/w-full) keeps the measurement exact
          in both normal and maximized layout. */}
      <div className="relative min-h-0 flex-1 overflow-hidden border border-border bg-black p-2">
        <div ref={hostRef} className="h-full w-full" />
        {/* A dead shell looks exactly like a live one — same text, same
            cursor — so blurring the scrollback makes "this is over" the
            first thing you see, with both next moves on what you're
            looking at.

            Absolutely positioned so it never enters FitAddon's measurement
            (see the wrapper's comment above) — it sizes from this
            wrapper's computed box, which an out-of-flow child doesn't
            affect.

            Deliberately NOT themed: everything under it is the terminal's
            `bg-black` in both modes, so a `--background`-keyed scrim would
            be near-invisible in dark mode and fight the black surface in
            light mode. */}
        {closed && !dismissed && (
          <div className="absolute inset-0 z-10 flex flex-col items-center justify-center gap-4 bg-black/60 backdrop-blur-[3px]">
            <span className="font-mono text-xs uppercase tracking-widest text-white/90">
              {closed}
            </span>
            <div className="flex gap-2">
              <Button size="sm" onClick={() => setGen((g) => g + 1)}>
                Start new session
              </Button>
              {/* Not "Close" — on an overlay covering a terminal that reads as
                  closing the terminal, which is the opposite of what it does. */}
              <Button size="sm" variant="outline" onClick={() => setDismissed(true)}>
                View output
              </Button>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
