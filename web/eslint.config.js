import js from "@eslint/js";
import globals from "globals";
import reactHooks from "eslint-plugin-react-hooks";
import reactRefresh from "eslint-plugin-react-refresh";
import tseslint from "typescript-eslint";
import { defineConfig, globalIgnores } from "eslint/config";

export default defineConfig([
  globalIgnores(["dist", "src/lib/openxet-wasm"]),
  {
    files: ["**/*.{ts,tsx}"],
    extends: [
      js.configs.recommended,
      tseslint.configs.recommended,
      reactHooks.configs.flat.recommended,
      reactRefresh.configs.vite,
    ],
    languageOptions: {
      ecmaVersion: 2020,
      globals: globals.browser,
    },
  },
  {
    files: ["src/components/ui/**/*.{ts,tsx}"],
    rules: {
      "react-refresh/only-export-components": "off",
    },
  },
  {
    // Vendored shadcn tree-view component (installed via the shadcn registry).
    // Treated like generated UI code: not held to our lint rules.
    files: [
      "src/components/tree-view.tsx",
      "src/components/tree-node.tsx",
      "src/components/tree-drop-indicator.tsx",
      "src/hooks/use-tree-*.ts",
      "src/lib/tree-*.ts",
    ],
    rules: {
      "react-hooks/refs": "off",
      "react-refresh/only-export-components": "off",
    },
  },
]);
