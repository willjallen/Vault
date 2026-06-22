module.exports = {
  extends: ["stylelint-config-standard"],
  ignoreFiles: ["**/node_modules/**", "**/dist/**", "**/build/**"],
  rules: {
    "color-hex-length": "short",
    "color-function-notation": null,
    "alpha-value-notation": null,
    "media-feature-range-notation": null,
    "declaration-no-important": true,
    "max-nesting-depth": 3,
    "selector-max-id": 0,
    "selector-max-type": 3,
    "no-descending-specificity": null,
    "shorthand-property-no-redundant-values": null
  }
};
