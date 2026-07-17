import { Buffer } from "node:buffer";
import assert from "node:assert/strict";
import test from "node:test";

import { build } from "esbuild";

globalThis.React = {
  createElement: (type, props, ...children) => ({ children, props: props || {}, type }),
};

const sourceUrl = new URL("../src/components/toolbar/Toolbar.js", import.meta.url);
const bundled = await build({
  bundle: true,
  entryPoints: [sourceUrl.pathname],
  format: "esm",
  platform: "node",
  write: false,
});
const moduleUrl = `data:text/javascript;base64,${Buffer.from(bundled.outputFiles[0].text).toString(
  "base64"
)}`;
const { Toolbar } = await import(moduleUrl);

function renderToolbar(logoutUrl) {
  return Toolbar({
    breadcrumbs: [],
    canGoBack: false,
    canGoForward: false,
    canGoUp: false,
    folder: "",
    logoutUrl,
  });
}

function elementsOfType(node, type, result = []) {
  if (Array.isArray(node)) {
    node.forEach((child) => elementsOfType(child, type, result));
    return result;
  }
  if (!node || typeof node !== "object") {
    return result;
  }
  if (node.type === type) {
    result.push(node);
  }
  elementsOfType(node.children, type, result);
  return result;
}

test("local logout uses a same-origin POST form", () => {
  const logoutUrl = "/logout?rd=%2FProject";
  const tree = renderToolbar(logoutUrl);
  const forms = elementsOfType(tree, "form");
  const logoutButtons = elementsOfType(tree, "button").filter(
    (button) => button.props["aria-label"] === "Log out"
  );

  assert.equal(forms.length, 1);
  assert.equal(forms[0].props.action, logoutUrl);
  assert.equal(forms[0].props.method, "post");
  assert.equal(forms[0].props.className, "logout-form");
  assert.equal(logoutButtons.length, 1);
  assert.equal(logoutButtons[0].props.type, "submit");
  assert.equal(
    elementsOfType(tree, "a").some((link) => link.props.href === logoutUrl),
    false
  );
});

test("external header-auth logout remains an ordinary GET link", () => {
  const logoutUrl = "https://auth.example.com/logout?rd=https%3A%2F%2Fvault.example.com%2F";
  const tree = renderToolbar(logoutUrl);
  const logoutLinks = elementsOfType(tree, "a").filter(
    (link) => link.props["aria-label"] === "Log out"
  );

  assert.equal(elementsOfType(tree, "form").length, 0);
  assert.equal(logoutLinks.length, 1);
  assert.equal(logoutLinks[0].props.href, logoutUrl);
});
