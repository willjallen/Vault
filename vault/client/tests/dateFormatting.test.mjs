import { Buffer } from "node:buffer";
import { readFile } from "node:fs/promises";
import assert from "node:assert/strict";
import test from "node:test";

const sourceUrl = new URL("../src/lib/utils.js", import.meta.url);
const source = await readFile(sourceUrl, "utf8");
const moduleUrl = `data:text/javascript;base64,${Buffer.from(source).toString("base64")}`;
const { formatDate } = await import(moduleUrl);

test(
  "canonical UTC timestamps render in the browser's local timezone",
  { concurrency: false },
  () => {
    /*
     * Runs the formatter in US Central time with a six-digit UTC timestamp. The displayed clock
     * must use the local offset while retaining a same-local-day semantic label.
     */
    const previousTimezone = process.env.TZ;
    process.env.TZ = "America/Chicago";
    try {
      assert.equal(
        formatDate(
          "2026-06-26T19:03:04.123456Z",
          "Invalid timestamp",
          new Date("2026-06-26T20:00:00.000000Z")
        ),
        "Today at 2:03 pm"
      );
    } finally {
      if (previousTimezone === undefined) {
        delete process.env.TZ;
      } else {
        process.env.TZ = previousTimezone;
      }
    }
  }
);
