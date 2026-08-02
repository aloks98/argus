// The app's ONE CodeBlock highlighter: registers exactly the grammars and
// themes an argus CodeBlock renders. Never import rnui's `code-block-full`
// (the full-bundle opt-in) — it references every shiki grammar/theme, which
// makes Vite emit ~355 lazy chunks (~12 MB of dist), all embedded into the
// argus binary via rust-embed.
//
// Registrations are dynamic imports, so each grammar/theme is its own lazy
// chunk fetched on first render — never entry-bundle weight.
//
// New language or theme in a CodeBlock? Register it here. An unregistered
// one REJECTS inside the highlighter, whose catch renders an empty block —
// visible, not silent.
import { createCodeBlockHighlighter } from "@e412/rnui-react";

export const codeHighlighter = createCodeBlockHighlighter({
  langs: {
    bash: () => import("@shikijs/langs/bash"),
    // Aliases call sites might reasonably pass for the same grammar.
    sh: () => import("@shikijs/langs/bash"),
    shell: () => import("@shikijs/langs/bash"),
  },
  // Must cover every theme a call site passes via CodeBlock's `themes`
  // prop: EnrollPage passes min-light/vesper (chosen over the
  // github-default pair to match the app's black/hazard-yellow identity).
  themes: {
    "min-light": () => import("@shikijs/themes/min-light"),
    vesper: () => import("@shikijs/themes/vesper"),
  },
});
