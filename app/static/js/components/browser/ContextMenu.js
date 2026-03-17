import { classNames } from "../../lib/utils.js";

const h = React.createElement;
const { useEffect, useRef } = React;

export function ContextMenu({ menu, onClose }) {
  const menuRef = useRef(null);

  useEffect(() => {
    if (!menu) {
      return undefined;
    }

    function handleClick(evt) {
      if (menuRef.current && !menuRef.current.contains(evt.target)) {
        onClose();
      }
    }

    function handleEscape(evt) {
      if (evt.key === "Escape") {
        onClose();
      }
    }

    function handleWheel() {
      onClose();
    }

    window.addEventListener("mousedown", handleClick);
    window.addEventListener("wheel", handleWheel, true);
    window.addEventListener("keydown", handleEscape);

    return () => {
      window.removeEventListener("mousedown", handleClick);
      window.removeEventListener("wheel", handleWheel, true);
      window.removeEventListener("keydown", handleEscape);
    };
  }, [menu, onClose]);

  if (!menu || !menu.items || menu.items.length === 0) {
    return null;
  }

  const maxX = window.innerWidth - 240;
  const maxY = window.innerHeight - 260;
  const style = {
    top: Math.max(8, Math.min(menu.y, maxY)),
    left: Math.max(8, Math.min(menu.x, maxX)),
  };

  return h(
    "div",
    { className: "context-menu", ref: menuRef, style, role: "menu" },
    menu.items.map((item, idx) => {
      if (!item) {
        return null;
      }
      if (item.type === "separator") {
        return h("div", { key: `sep-${idx}`, className: "context-separator" });
      }
      return h(
        "button",
        {
          key: `${item.label || "item"}-${idx}`,
          className: classNames(
            "context-item",
            item.danger ? "danger" : "",
            item.disabled ? "disabled" : ""
          ),
          type: "button",
          title: item.disabled && item.note ? item.note : undefined,
          onClick: () => {
            if (item.disabled) {
              return;
            }
            onClose();
            if (item.action) {
              item.action();
            }
          },
        },
        h("span", { className: "context-label" }, item.label)
      );
    })
  );
}
