import { Buffer } from "node:buffer";
import { readFile } from "node:fs/promises";
import assert from "node:assert/strict";
import test from "node:test";

const sourceUrl = new URL("../../app/static/js/lib/menuTargets.js", import.meta.url);
const source = await readFile(sourceUrl, "utf8");
const moduleUrl = `data:text/javascript;base64,${Buffer.from(source).toString("base64")}`;
const { markFavoriteContextTarget } = await import(moduleUrl);

test("selected favorite document keeps its favorite context marker", () => {
  const selectedItems = [{ favorite: false, id: 42, name: "clip.mov", type: "document" }];
  const targetItem = { favorite: true, id: 42, name: "clip.mov", type: "document" };

  assert.deepEqual(markFavoriteContextTarget(selectedItems, targetItem), [
    { favorite: true, id: 42, name: "clip.mov", type: "document" },
  ]);
});

test("selected favorite folder keeps its favorite context marker", () => {
  const selectedItems = [{ favorite: false, id: 7, path: "Art", type: "folder" }];
  const targetItem = { favorite: true, id: 7, path: "Art", type: "folder" };

  assert.deepEqual(markFavoriteContextTarget(selectedItems, targetItem), [
    { favorite: true, id: 7, path: "Art", type: "folder" },
  ]);
});

test("favorite marker only applies to the clicked target", () => {
  const selectedItems = [
    { favorite: false, id: 1, name: "a.txt", type: "document" },
    { favorite: false, id: 2, name: "b.txt", type: "document" },
  ];
  const targetItem = { favorite: true, id: 2, name: "b.txt", type: "document" };

  assert.deepEqual(markFavoriteContextTarget(selectedItems, targetItem), [
    { favorite: false, id: 1, name: "a.txt", type: "document" },
    { favorite: true, id: 2, name: "b.txt", type: "document" },
  ]);
});
