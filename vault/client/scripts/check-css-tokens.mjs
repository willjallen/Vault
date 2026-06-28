import fs from "node:fs";
import path from "node:path";

const clientRoot = path.resolve(import.meta.dirname, "..");
const cssPaths = [
  path.join(clientRoot, "styles/palette.css"),
  path.join(clientRoot, "styles/styles.css"),
];
const colorPattern =
  /#[0-9a-fA-F]{3,8}\b|(?:rgba?|hsla?)\s*\(|(?:linear|radial|conic)-gradient\s*\(/g;
const allowedSelectors = new Set([
  ":root",
  ':root[data-theme="dark"]',
  ':root[data-palette="winui"]',
  ':root[data-palette="winui"][data-theme="dark"]',
]);

function lineColumn(css, index) {
  const lines = css.slice(0, index).split("\n");
  return { line: lines.length, column: lines.at(-1).length + 1 };
}

function isWithinAllowedRange(index, allowedRanges) {
  return allowedRanges.some(([start, end]) => index >= start && index < end);
}

function scanCss(cssPath) {
  const css = fs.readFileSync(cssPath, "utf8");
  const allowedRanges = [];
  const selectorViolations = [];
  const stack = [];
  let selectorBoundary = 0;

  for (let index = 0; index < css.length; index += 1) {
    if (css[index] === "/" && css[index + 1] === "*") {
      const commentEnd = css.indexOf("*/", index + 2);
      index = commentEnd === -1 ? css.length : commentEnd + 1;
      continue;
    }

    if (css[index] === ";" && stack.length === 0) {
      selectorBoundary = index + 1;
      continue;
    }

    if (css[index] === "{") {
      const selectorStart = selectorBoundary;
      const selector = css.slice(selectorStart, index).trim();
      const tokenBlock = allowedSelectors.has(selector);

      if (
        (selector.includes(':root[data-theme="dark"] ') ||
          selector.includes(':root[data-palette="winui"] ')) &&
        !tokenBlock
      ) {
        selectorViolations.push({ selector, ...lineColumn(css, selectorStart) });
      }

      stack.push({ selector, tokenBlock, start: index + 1 });
      selectorBoundary = index + 1;
      continue;
    }

    if (css[index] === "}") {
      const block = stack.pop();

      if (block?.tokenBlock) {
        allowedRanges.push([block.start, index]);
      }

      selectorBoundary = index + 1;
    }
  }

  const colorViolations = [];
  let match = null;
  colorPattern.lastIndex = 0;

  while ((match = colorPattern.exec(css)) !== null) {
    if (isWithinAllowedRange(match.index, allowedRanges)) {
      continue;
    }

    colorViolations.push({ value: match[0], ...lineColumn(css, match.index) });
  }

  return { colorViolations, selectorViolations };
}

let hasViolations = false;

for (const cssPath of cssPaths) {
  const { colorViolations, selectorViolations } = scanCss(cssPath);

  if (!selectorViolations.length && !colorViolations.length) {
    continue;
  }

  hasViolations = true;
  console.error(`${cssPath} contains theme values outside the canonical token blocks.`);

  for (const violation of selectorViolations) {
    console.error(
      `  ${violation.line}:${violation.column} dark theme component selector: ${violation.selector}`
    );
  }

  for (const violation of colorViolations) {
    console.error(`  ${violation.line}:${violation.column} raw color value: ${violation.value}`);
  }
}

if (hasViolations) {
  process.exit(1);
}
