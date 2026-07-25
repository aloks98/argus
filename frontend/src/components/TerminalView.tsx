// Interactive shell over a WebSocket. We own the xterm instance and the socket;
// Binary frames carry PTY bytes both ways. Text frames are asymmetric: browser
// -> server they're the JSON resize control, but the sole server -> browser
// Text frame is a human-readable reason (e.g. "agent not connected") sent
// immediately before the socket closes because no PTY was ever opened — there
// is no PtyOutput to carry that failure, so this is the only channel for it.
// Maximize is a LAYOUT toggle on the same mounted terminal — never a remount,
// which would drop the shell. No auto-reconnect: a closed shell is gone, so we
// surface a "Start new session" button rather than silently opening a fresh
// one behind the same view.
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

    // The most specific close reason seen so far. onmessage (a server-sent
    // reason like "agent not connected") and onerror can both fire before the
    // close event that inevitably follows them, and neither must be clobbered
    // by onclose's generic fallback — so whichever handler runs first records
    // its reason here, and later handlers only fill in a reason if this is
    // still unset.
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
      // existing "session closed" wording. An abnormal close the server
      // tagged with a CloseEvent.reason wins over that generic text; either
      // way, a reason already recorded by onmessage/onerror above wins over
      // both, since it is the most specific thing we've heard.
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

    // Keystrokes -> Binary frames.
    const dataSub = term.onData((s) => {
      if (ws.readyState === WebSocket.OPEN) ws.send(enc.encode(s));
    });

    // Refit on container resize (covers maximize/restore too).
    const ro = new ResizeObserver(() => sendResize());
    ro.observe(host);

    return () => {
      dataSub.dispose();
      ro.disconnect();
      // Detach handlers before/after closing: browser close events are
      // asynchronous, so without this a stale onclose from THIS (dead) socket
      // can still fire after "Start new session" has bumped `gen` and a new
      // effect run has opened a live socket — clobbering the new session's
      // banner with this one's leftover message. `ws.close()` still runs so
      // the server learns to kill the shell; only the callbacks are removed.
      ws.onopen = null;
      ws.onmessage = null;
      ws.onclose = null;
      ws.onerror = null;
      ws.close();
      termRef.current = null;
      term.dispose();
    };
  }, [machineId, gen]);

  // Restore keyboard focus to the terminal after a maximize/restore toggle.
  // Maximize only ever changes the wrapper's layout class -- the same
  // mounted `Terminal` and its hidden input stay put -- so a plain
  // `.focus()` here is enough; there is no remount to race against. Also
  // fires (harmlessly) on initial mount, redundant with the `term.focus()`
  // call above but not incorrect.
  useEffect(() => {
    termRef.current?.focus();
  }, [maximized]);

  return (
    <div className={maximized ? "fixed inset-0 z-50 flex flex-col bg-background p-2" : "flex h-[70vh] flex-col"}>
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
      {/* The border is load-bearing, not decoration: the terminal surface is
          always `bg-black`, and in dark mode the page background is #000000
          too — without an edge the terminal is invisible against the page and
          the prompt appears to float in empty space. `--border` resolves to
          #242424 in dark and #D4D4D8 in light, so one border works for both.
          `bg-black` and the padding live here too, so the inset gap reads as
          part of the terminal surface rather than as a gap showing the page
          through — and, like the border, padding on the mount div itself
          would corrupt FitAddon's measurement, whereas padding here is
          outside the box it measures.
          It lives on THIS wrapper, one level above the div xterm mounts
          into, deliberately: `FitAddon.proposeDimensions()` measures
          `term.element.parentElement` via `getComputedStyle(...).height`/
          `.width`, and under this app's global `box-sizing: border-box`
          reset that computed value INCLUDES the element's own border. Put
          the border directly on the mount div and FitAddon would overcount
          the available space by the border's thickness on each axis (it
          only subtracts the xterm-rendered element's padding, never a
          border on its own parent), nudging cols/rows up by a fraction of a
          cell and clipping the last row/column against `overflow-hidden`.
          Keeping the mount div itself borderless — sized to fill this
          wrapper's content box via h-full/w-full — keeps that measurement
          exact in both normal and maximized layout. */}
      <div className="relative min-h-0 flex-1 overflow-hidden border border-border bg-black p-2">
        <div ref={hostRef} className="h-full w-full" />
        {/* A dead shell otherwise looks exactly like a live one — same text,
            same cursor — and the only tell is a line of small print in the
            header. Blurring the scrollback makes "this is over" the first
            thing you see, and puts both next moves on the thing you are
            looking at.

            Absolutely positioned so it never enters FitAddon's measurement:
            it sizes from this wrapper's computed box, which an out-of-flow
            child does not affect.

            Deliberately NOT themed. Everything under it is the terminal's
            `bg-black` surface in light mode and dark mode alike, so the
            contrast this needs is against black in both — a scrim keyed to
            `--background` would be near-invisible in dark mode and would
            fight the black surface in light mode. */}
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
