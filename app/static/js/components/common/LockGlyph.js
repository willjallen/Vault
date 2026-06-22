const h = React.createElement;

export function LockGlyph({ className = "" }) {
  return h(
    "svg",
    {
      "aria-hidden": "true",
      className,
      viewBox: "0 0 20 20",
      width: 15,
      height: 15,
      fill: "none",
      stroke: "currentColor",
      strokeWidth: 1.8,
    },
    [
      h("rect", { x: 4, y: 8, width: 12, height: 9, rx: 2 }),
      h("path", { d: "M7 8V6a3 3 0 1 1 6 0v2" }),
    ]
  );
}
