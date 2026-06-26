import { readdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";

import * as esbuild from "esbuild";

const root = process.cwd();
const sourceOutdir = path.join(root, "app/static/dist");
const checkMode = process.argv.includes("--check");
const outdir = checkMode ? path.join(tmpdir(), `vault-assets-check-${process.pid}`) : sourceOutdir;

const entries = {
  app: "app/static/js/main.js",
  styles: "app/static/styles.css",
};
const manifestKeys = {
  app: "app.js",
  styles: "styles.css",
};

function normalizePath(value) {
  return value.split(path.sep).join("/");
}

async function listFiles(dir) {
  const files = [];

  async function visit(current) {
    let children = [];
    try {
      children = await readdir(current);
    } catch {
      return;
    }
    for (const child of children) {
      const childPath = path.join(current, child);
      const childStat = await stat(childPath);
      if (childStat.isDirectory()) {
        await visit(childPath);
        continue;
      }
      files.push(normalizePath(path.relative(dir, childPath)));
    }
  }

  await visit(dir);
  return files.sort();
}

async function sameFile(left, right) {
  try {
    const [leftBytes, rightBytes] = await Promise.all([readFile(left), readFile(right)]);
    return leftBytes.equals(rightBytes);
  } catch {
    return false;
  }
}

async function assertDirectoriesMatch(expectedDir, actualDir) {
  const [expectedFiles, actualFiles] = await Promise.all([
    listFiles(expectedDir),
    listFiles(actualDir),
  ]);
  const expectedSet = new Set(expectedFiles);
  const actualSet = new Set(actualFiles);
  const missing = expectedFiles.filter((file) => !actualSet.has(file));
  const extra = actualFiles.filter((file) => !expectedSet.has(file));
  const changed = [];

  for (const file of expectedFiles) {
    if (!actualSet.has(file)) {
      continue;
    }
    const matches = await sameFile(path.join(expectedDir, file), path.join(actualDir, file));
    if (!matches) {
      changed.push(file);
    }
  }

  if (missing.length || extra.length || changed.length) {
    const details = [
      ...missing.map((file) => `missing ${file}`),
      ...extra.map((file) => `extra ${file}`),
      ...changed.map((file) => `changed ${file}`),
    ];
    throw new Error(`Static asset bundle is stale:\n${details.join("\n")}`);
  }
}

async function buildAssets(targetDir) {
  await rm(targetDir, { force: true, recursive: true });
  const result = await esbuild.build({
    assetNames: "assets/[name]-[hash]",
    bundle: true,
    define: {
      "process.env.NODE_ENV": '"production"',
    },
    entryNames: "[name]-[hash]",
    entryPoints: entries,
    format: "esm",
    inject: ["scripts/react-globals.mjs"],
    legalComments: "none",
    metafile: true,
    minify: true,
    outdir: targetDir,
    platform: "browser",
    target: ["es2020"],
  });

  const manifest = {};
  for (const [outputPath, metadata] of Object.entries(result.metafile.outputs)) {
    if (!metadata.entryPoint) {
      continue;
    }
    const entryName = Object.entries(entries).find(
      ([, entryPoint]) => normalizePath(metadata.entryPoint) === entryPoint
    )?.[0];
    const manifestKey = entryName ? manifestKeys[entryName] : null;
    if (!manifestKey) {
      continue;
    }
    const relativeOutput = normalizePath(path.relative(targetDir, path.resolve(root, outputPath)));
    manifest[manifestKey] = `/static/dist/${relativeOutput}`;
  }

  await writeFile(
    path.join(targetDir, "manifest.json"),
    `${JSON.stringify(manifest, Object.keys(manifest).sort(), 2)}\n`
  );
}

try {
  await buildAssets(outdir);
  if (checkMode) {
    await assertDirectoriesMatch(outdir, sourceOutdir);
    await rm(outdir, { force: true, recursive: true });
  }
} catch (error) {
  if (checkMode) {
    await rm(outdir, { force: true, recursive: true });
  }
  console.error(error instanceof Error ? error.message : error);
  process.exitCode = 1;
}
