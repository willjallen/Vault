import { classNames } from "../../lib/utils.js";
import { FileIcon } from "./FileIcon.js";

const { useEffect, useMemo, useState } = React;
const h = React.createElement;
const PREVIEW_RETRY_LIMIT = 6;

export function previewRetryDelay(attempt) {
  return Math.min(1000 * 2 ** Math.max(0, Number(attempt) - 1), 8000);
}

export function readyPreviewVariants(item) {
  const preview = item?.visual?.preview;
  if (preview?.status !== "ready" || !Array.isArray(preview.variants)) {
    return [];
  }
  return preview.variants
    .filter(
      (variant) =>
        variant &&
        typeof variant.url === "string" &&
        variant.url.length > 0 &&
        Number.isFinite(Number(variant.width)) &&
        Number(variant.width) > 0
    )
    .slice()
    .sort((left, right) => Number(left.width) - Number(right.width));
}

export function previewSourceSet(variants) {
  return variants.map((variant) => `${variant.url} ${Number(variant.width)}w`).join(", ");
}

export function previewVariantForSize(variants, desiredSize) {
  const target = Math.max(1, Number(desiredSize) || 1);
  return variants.find((variant) => Number(variant.width) >= target) || variants.at(-1) || null;
}

// Optional preview fields intentionally collapse to a static icon at every invalid boundary.
// eslint-disable-next-line complexity
export function AssetVisual({
  className = "",
  desiredSize = 24,
  item,
  kind = item?.type === "folder" ? "folder" : "file",
}) {
  const variants = useMemo(() => readyPreviewVariants(item), [item]);
  const selectedVariant = previewVariantForSize(variants, desiredSize);
  const previewKey = selectedVariant?.url || "";
  const preview = item?.visual?.preview;
  const [failedPreview, setFailedPreview] = useState("");
  const [retryAttempt, setRetryAttempt] = useState(0);

  useEffect(() => {
    setFailedPreview("");
    setRetryAttempt(0);
  }, [preview, previewKey]);

  useEffect(() => {
    if (failedPreview !== previewKey || !previewKey || retryAttempt > PREVIEW_RETRY_LIMIT) {
      return undefined;
    }
    const timer = window.setTimeout(() => setFailedPreview(""), previewRetryDelay(retryAttempt));
    return () => window.clearTimeout(timer);
  }, [failedPreview, previewKey, retryAttempt]);

  const showPreview = selectedVariant && failedPreview !== previewKey;
  const fallback = h(FileIcon, {
    color: item?.color || "",
    fileName: item?.name || "",
    folderIcon: item?.icon || "",
    iconKey: item?.visual?.icon_key || "",
    kind,
    size: Math.max(18, Number(desiredSize) || 18),
  });

  return h(
    "span",
    {
      className: classNames("asset-visual", showPreview ? "has-preview" : "fallback", className),
      "data-preview-status": preview?.status || undefined,
    },
    showPreview
      ? h("img", {
          alt: "",
          decoding: "async",
          draggable: false,
          height: Number(selectedVariant.height) || Number(selectedVariant.width),
          loading: "lazy",
          onError: () => {
            setFailedPreview(previewKey);
            setRetryAttempt((current) => current + 1);
          },
          sizes: `${Math.max(1, Math.round(Number(desiredSize) || 1))}px`,
          src: selectedVariant.url,
          srcSet: previewSourceSet(variants),
          width: Number(selectedVariant.width),
        })
      : fallback
  );
}
