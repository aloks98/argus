// Drop-in replacement for the top-level `shiki` package, wired in via a
// vite alias (see vite.config.ts). rnui's CodeBlock imports `codeToHtml`
// from bare `shiki`, whose bundle auto-registers EVERY grammar and theme —
// +70 kB gzip on the main chunk and ~12 MB of lazy chunks in dist/, all of
// it embedded into the `argus` binary by rust-embed. Same treatment echarts
// got (stubbed to just what's used): this registers only what Argus renders —
// bash, plus the two themes rnui's CodeBlock defaults to.
//
// Everything is imported DYNAMICALLY inside the first call: the only
// CodeBlock in the app lives in the enroll page's token-minted dialog, so
// the grammar/engine/themes belong in a lazy chunk fetched on first render,
// not in the main bundle every page pays for.
//
// Adding a CodeBlock in a NEW language? Register its grammar here, or the
// block renders as plain text (shiki treats unknown/`text` as plain rather
// than throwing — so this fails soft, visibly, not with a crash).
import type { HighlighterCore } from "shiki/core";

let highlighter: Promise<HighlighterCore> | undefined;

function load(): Promise<HighlighterCore> {
  // On failure the cache is CLEARED before rethrowing: a transient chunk-load
  // error (dev-server restart, flaky network) must not pin every later
  // CodeBlock to a cached rejection — CodeBlock's catch renders plain text
  // with no error surfaced, so a permanently poisoned cache would look
  // exactly like "highlighting silently stopped working".
  highlighter ??= build().catch((err: unknown) => {
    highlighter = undefined;
    throw err;
  });
  return highlighter;
}

function build(): Promise<HighlighterCore> {
  return (async () => {
    // Themes must match what call sites pass to CodeBlock's `themes` prop
    // (EnrollPage passes min-light/vesper — chosen over CodeBlock's
    // github-default pair to match the app's black/hazard-yellow identity).
    const [{ createHighlighterCore }, { createJavaScriptRegexEngine }, bash, light, dark] =
      await Promise.all([
        import("shiki/core"),
        import("shiki/engine/javascript"),
        import("@shikijs/langs/bash"),
        import("@shikijs/themes/min-light"),
        import("@shikijs/themes/vesper"),
      ]);
    return createHighlighterCore({
      langs: [bash.default],
      themes: [light.default, dark.default],
      engine: createJavaScriptRegexEngine(),
    });
  })();
}

type CodeToHtmlOptions = Parameters<HighlighterCore["codeToHtml"]>[1];

export async function codeToHtml(code: string, options: CodeToHtmlOptions): Promise<string> {
  const h = await load();
  const lang = String(options.lang ?? "text");
  // An unregistered language degrades to plain text instead of rejecting —
  // CodeBlock's catch-path would otherwise swallow the error and render
  // nothing, which reads as a broken dialog.
  const known = h.getLoadedLanguages().includes(lang);
  return h.codeToHtml(code, { ...options, lang: known ? lang : "text" });
}
