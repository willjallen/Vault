import { rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";

import * as esbuild from "esbuild";

const clientRoot = path.resolve(import.meta.dirname, "..");
const sourceOutdir = path.join(clientRoot, "dist");
const checkMode = process.argv.includes("--check");
const outdir = checkMode ? path.join(tmpdir(), `vault-assets-check-${process.pid}`) : sourceOutdir;

const entries = {
  app: path.join(clientRoot, "src/main.js"),
  styles: path.join(clientRoot, "styles/styles.css"),
};
const manifestKeys = {
  app: "app.js",
  styles: "styles.css",
};

function normalizePath(value) {
  return value.split(path.sep).join("/");
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
    inject: [path.join(clientRoot, "scripts/react-globals.mjs")],
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
      ([, entryPoint]) =>
        normalizePath(path.resolve(metadata.entryPoint)) === normalizePath(entryPoint)
    )?.[0];
    const manifestKey = entryName ? manifestKeys[entryName] : null;
    if (!manifestKey) {
      continue;
    }
    const relativeOutput = normalizePath(path.relative(targetDir, path.resolve(outputPath)));
    manifest[manifestKey] = `/static/dist/${relativeOutput}`;
  }

  await writeFile(
    path.join(targetDir, "manifest.json"),
    `${JSON.stringify(manifest, Object.keys(manifest).sort(), 2)}\n`
  );
  return manifest;
}

async function assertBuiltAssets(targetDir, manifest) {
  for (const key of Object.values(manifestKeys)) {
    const assetUrl = manifest[key];
    if (!assetUrl) {
      throw new Error(`Static asset manifest is missing ${key}`);
    }
    if (!assetUrl.startsWith("/static/dist/")) {
      throw new Error(`Static asset manifest entry ${key} must reference /static/dist`);
    }
    const relativeOutput = assetUrl.replace("/static/dist/", "");
    const assetPath = path.join(targetDir, relativeOutput);
    const assetStat = await stat(assetPath);
    if (!assetStat.isFile() || assetStat.size <= 0) {
      throw new Error(`Static asset ${key} is empty or invalid`);
    }
  }
}

try {
  const manifest = await buildAssets(outdir);
  await assertBuiltAssets(outdir, manifest);
  if (checkMode) {
    await rm(outdir, { force: true, recursive: true });
  }
} catch (error) {
  if (checkMode) {
    await rm(outdir, { force: true, recursive: true });
  }
  console.error(error instanceof Error ? error.message : error);
  process.exitCode = 1;
}
