const js = require("@eslint/js");
const globals = require("globals");
const importPlugin = require("eslint-plugin-import");
const jsxA11y = require("eslint-plugin-jsx-a11y");
const prettier = require("eslint-config-prettier/flat");
const react = require("eslint-plugin-react");
const reactHooks = require("eslint-plugin-react-hooks");
const security = require("eslint-plugin-security");

module.exports = [
  {
    ignores: ["node_modules/", "dist/", "build/", "coverage/", ".cache/"],
  },
  {
    files: ["src/**/*.js"],
    languageOptions: {
      ecmaVersion: "latest",
      sourceType: "module",
      parserOptions: {
        ecmaFeatures: {
          jsx: true,
        },
      },
      globals: {
        ...globals.browser,
        React: "readonly",
      },
    },
    settings: {
      react: {
        version: "18.3",
      },
    },
  },
  js.configs.recommended,
  importPlugin.flatConfigs.errors,
  importPlugin.flatConfigs.warnings,
  react.configs.flat.recommended,
  jsxA11y.flatConfigs.strict,
  security.configs.recommended,
  {
    files: ["src/**/*.js"],
    plugins: {
      "react-hooks": reactHooks,
    },
    rules: {
      "max-lines": ["error", { max: 950, skipBlankLines: true, skipComments: true }],
      "max-depth": ["error", 4],
      "max-params": ["error", 6],
      "max-statements": ["error", 120],
      complexity: ["error", 25],
      "no-warning-comments": ["error"],
      "no-console": ["error", { allow: ["warn", "error"] }],
      eqeqeq: ["error", "smart"],
      curly: ["error", "all"],
      "prefer-const": "error",
      "no-implicit-coercion": "error",
      "no-shadow": ["error", { builtinGlobals: true, hoist: "all" }],
      "no-param-reassign": ["error", { props: false }],
      "no-unused-vars": [
        "error",
        { argsIgnorePattern: "^_", varsIgnorePattern: "^_", caughtErrorsIgnorePattern: "^_" },
      ],
      "react/prop-types": "off",
      "react/react-in-jsx-scope": "off",
      "react/jsx-no-target-blank": ["error", { allowReferrer: false }],
      "react/function-component-definition": [
        "error",
        { namedComponents: "function-declaration", unnamedComponents: "arrow-function" },
      ],
      "react-hooks/exhaustive-deps": "error",
      "react-hooks/rules-of-hooks": "error",
      "security/detect-non-literal-fs-filename": "off",
    },
  },
  prettier,
];
