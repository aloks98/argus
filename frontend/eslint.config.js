// ESLint 9 flat config.
import js from "@eslint/js";
import tseslint from "typescript-eslint";
import reactHooks from "eslint-plugin-react-hooks";

export default tseslint.config(
  {
    ignores: ["dist/**", "node_modules/**"],
  },
  js.configs.recommended,
  // NON-type-aware: `tseslint.configs.recommended`, not
  // `recommendedTypeChecked`. Type-aware linting needs a
  // parserOptions.project wired to tsconfig.json and is measurably slower
  // per file (it type-checks rather than just parses) — that cost isn't
  // worth paying to get `npm run lint` running in CI today. Turning it on
  // is a deliberate later step, not an oversight.
  tseslint.configs.recommended,
  {
    plugins: {
      "react-hooks": reactHooks,
    },
    rules: {
      // Only these two, spelled out explicitly rather than spread from
      // `reactHooks.configs.recommended`: eslint-plugin-react-hooks 7.x
      // merged in the React Compiler's lint rules (purity,
      // set-state-in-render, immutability, etc.) under that preset, which
      // assume/enforce Compiler constraints this codebase doesn't opt into.
      "react-hooks/rules-of-hooks": "error",
      "react-hooks/exhaustive-deps": "warn",
      // Codifies a convention already in use (e.g. TimeSeriesChart.tsx's
      // `(_u: uPlot, vals: ...)` callback params): a leading underscore
      // marks a parameter as intentionally unused, most often to satisfy a
      // callback shape (uPlot's axis formatters, echarts' stub `use(_)`)
      // whose signature the caller controls, not this codebase.
      "@typescript-eslint/no-unused-vars": ["error", { argsIgnorePattern: "^_" }],
    },
  },
  // eqeqeq is intentionally NOT enabled. SystemCard.tsx (`row()`) and
  // MachineDetailPage.tsx (`uptime`, `error != null`, etc.) use loose
  // `== null` / `!= null` DELIBERATELY: proto fields added in a slice are
  // additive, so an older/newer frontend-server pairing sees `undefined`
  // where the other sees `null`, and strict `!==` would let the mismatched
  // one slip through and render "undefined" instead of omitting the row. If
  // eqeqeq is ever turned on, it MUST be
  // `["error", "always", { "null": "ignore" }]` — never the bare "always"
  // form, which would flag every one of those checks.
);
