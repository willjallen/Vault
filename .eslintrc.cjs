module.exports = {
  root: true,
  env: {
    browser: true,
    es2021: true
  },
  parserOptions: {
    ecmaVersion: "latest",
    sourceType: "module",
    ecmaFeatures: {
      jsx: true
    }
  },
  plugins: ["react", "react-hooks", "jsx-a11y", "import", "security"],
  extends: [
    "eslint:recommended",
    "plugin:react/recommended",
    "plugin:react-hooks/recommended",
    "plugin:jsx-a11y/strict",
    "plugin:import/errors",
    "plugin:import/warnings",
    "plugin:security/recommended",
    "prettier"
  ],
  settings: {
    react: {
      version: "detect"
    }
  },
  globals: {
    React: "readonly"
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
      { argsIgnorePattern: "^_", varsIgnorePattern: "^_", caughtErrorsIgnorePattern: "^_" }
    ],
    "react/prop-types": "off",
    "react/react-in-jsx-scope": "off",
    "react/jsx-no-target-blank": ["error", { allowReferrer: false }],
    "react/function-component-definition": [
      "error",
      { namedComponents: "function-declaration", unnamedComponents: "arrow-function" }
    ],
    "security/detect-non-literal-fs-filename": "off"
  },
  ignorePatterns: ["node_modules/", "dist/", "build/", "coverage/", ".cache/"]
};
