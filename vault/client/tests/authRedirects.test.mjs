import { Buffer } from "node:buffer";
import { readFile } from "node:fs/promises";
import assert from "node:assert/strict";
import test from "node:test";

const sourceUrl = new URL("../src/lib/authRedirects.js", import.meta.url);
const source = await readFile(sourceUrl, "utf8");
const moduleUrl = `data:text/javascript;base64,${Buffer.from(source).toString("base64")}`;
const { authRedirectUrl, authReturnTarget } = await import(moduleUrl);

const location = {
  hash: "#history",
  href: "https://vault.example.com/Project/Plan?tab=activity#history",
  pathname: "/Project/Plan",
  search: "?tab=activity",
};

test("local auth routes use an origin-relative return target", () => {
  assert.equal(authReturnTarget(location, false), "/Project/Plan?tab=activity#history");
  assert.equal(
    authRedirectUrl({ action: "login", authMode: "oidc", baseDomain: "", location }),
    "/login?rd=%2FProject%2FPlan%3Ftab%3Dactivity%23history"
  );
  assert.equal(
    authRedirectUrl({ action: "logout", authMode: "oidc", baseDomain: "", location }),
    "/logout?rd=%2FProject%2FPlan%3Ftab%3Dactivity%23history"
  );
});

test("external header auth routes retain the absolute return target", () => {
  assert.equal(authReturnTarget(location, true), location.href);
  assert.equal(
    authRedirectUrl({
      action: "login",
      authMode: "headers",
      baseDomain: "example.com",
      location,
    }),
    "https://auth.example.com/?rd=https%3A%2F%2Fvault.example.com%2FProject%2FPlan%3Ftab%3Dactivity%23history"
  );
  assert.equal(
    authRedirectUrl({
      action: "logout",
      authMode: "headers",
      baseDomain: "example.com",
      location,
    }),
    "https://auth.example.com/logout?rd=https%3A%2F%2Fvault.example.com%2FProject%2FPlan%3Ftab%3Dactivity%23history"
  );
});

test("local return targets fall back to the root path", () => {
  assert.equal(
    authReturnTarget(
      { hash: "", href: "https://vault.example.com", pathname: "", search: "" },
      false
    ),
    "/"
  );
});
