// Alias for bare `shiki` (see vite.config.ts): the real package
// auto-registers every grammar/theme (~12 MB, embedded into the argus
// binary via rust-embed). This registers only bash + the two used themes.
//
// Imports are DYNAMIC inside the first call, not top-level: the one
// CodeBlock in the app (enroll page's token dialog) should cost a lazy
// chunk on first render, not weight on every page's main bundle.
//
// New language in a CodeBlock? Register its grammar here — unregistered
// languages render as plain text (shiki fails soft, not with a throw).
import type { HighlighterCore } from "shiki/core";

let highlighter: Promise<HighlighterCore> | undefined;

function load(): Promise<HighlighterCore> {
  // Clear the cache before rethrowing: a transient failure (e.g. chunk-load)
  // must not pin every later call to a cached rejection — CodeBlock hides
  // errors, so that would look identical to "highlighting silently broke".
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
