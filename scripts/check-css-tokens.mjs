import fs from "node:fs";

const cssPath = "app/static/styles.css";
const css = fs.readFileSync(cssPath, "utf8");
const colorPattern =
  /#[0-9a-fA-F]{3,8}\b|(?:rgba?|hsla?)\s*\(|(?:linear|radial|conic)-gradient\s*\(/g;
const allowedSelectors = new Set([":root", ':root[data-theme="dark"]']);
const allowedRanges = [];
const selectorViolations = [];
const stack = [];
let selectorBoundary = 0;

function lineColumn(index) {
  const lines = css.slice(0, index).split("\n");
  return { line: lines.length, column: lines.at(-1).length + 1 };
}

function isWithinAllowedRange(index) {
  return allowedRanges.some(([start, end]) => index >= start && index < end);
}

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
      selector.startsWith(':root[data-theme="dark"] ') ||
      selector.includes(':root[data-theme="dark"] ')
    ) {
      selectorViolations.push({ selector, ...lineColumn(selectorStart) });
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

while ((match = colorPattern.exec(css)) !== null) {
  if (isWithinAllowedRange(match.index)) {
    continue;
  }

  colorViolations.push({ value: match[0], ...lineColumn(match.index) });
}

if (selectorViolations.length || colorViolations.length) {
  console.error(`${cssPath} contains theme values outside the canonical token blocks.`);

  for (const violation of selectorViolations) {
    console.error(
      `  ${violation.line}:${violation.column} dark theme component selector: ${violation.selector}`
    );
  }

  for (const violation of colorViolations) {
    console.error(`  ${violation.line}:${violation.column} raw color value: ${violation.value}`);
  }

  process.exit(1);
}
