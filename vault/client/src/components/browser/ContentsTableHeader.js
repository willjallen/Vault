import { CONTENTS_VIEW_MODES } from "../../lib/contentsView.js";
import { classNames } from "../../lib/utils.js";
import { Icon } from "../common/Icon.js";
import { COLUMN_RESIZE_HANDLES } from "./contentColumns.js";

const h = React.createElement;
const NAME_COLUMN = { key: "name", label: "Name", className: "name", defaultDirection: "asc" };
const DETAIL_SORT_COLUMNS = [
  { key: "modified", label: "Modified", className: "modified", defaultDirection: "desc" },
  { key: "user", label: "User", className: "user", defaultDirection: "asc" },
  { key: "size", label: "Size", className: "size", defaultDirection: "desc" },
  { key: "ttl", label: "Status", className: "status", defaultDirection: "asc" },
];

function ContentsSortButton({ column, sort, onSortChange }) {
  const active = sort?.key === column.key;
  const direction = active ? sort.direction : column.defaultDirection;
  return h(
    "button",
    {
      type: "button",
      className: classNames(
        "contents-sort-button",
        `contents-sort-${column.className}`,
        active ? "active" : ""
      ),
      "aria-sort": active ? (direction === "desc" ? "descending" : "ascending") : "none",
      onClick: (evt) => {
        evt.stopPropagation();
        onSortChange?.(column.key);
      },
    },
    [
      h("span", { key: "label" }, column.label),
      h(Icon, {
        className: classNames("contents-sort-arrow", active ? "active" : "preview"),
        icon: direction === "desc" ? "arrow-down" : "arrow-up",
        key: "icon",
        size: 10,
      }),
    ]
  );
}

function ContentsHeaderCell({ children, columnKey, resizeHandle, resizeHandlers }) {
  return h(
    "div",
    {
      className: classNames(
        "contents-head-cell",
        columnKey === "name" ? "name-column" : "",
        columnKey === "actions" ? "actions-column" : "",
        columnKey !== "name" && columnKey !== "actions" ? "detail-column" : ""
      ),
      "data-column-key": columnKey,
    },
    [
      h(React.Fragment, { key: "content" }, children),
      resizeHandle
        ? h("button", {
            "aria-label": `Resize ${resizeHandle.left} and ${resizeHandle.right} columns`,
            className: "contents-column-resizer",
            key: "resize",
            onClick: (evt) => evt.stopPropagation(),
            onPointerCancel: resizeHandlers.end,
            onPointerDown: (evt) => resizeHandlers.start(resizeHandle, evt),
            onPointerMove: resizeHandlers.move,
            onPointerUp: resizeHandlers.end,
            title: "Resize column",
            type: "button",
          })
        : null,
    ]
  );
}

export function ContentsTableHeader({
  allVisibleSelected,
  headerRef,
  mode,
  onSelectAllChange,
  onSortChange,
  resizeHandlers,
  sort,
  visibleCount,
}) {
  if (mode !== CONTENTS_VIEW_MODES.DETAILS) {
    return null;
  }
  return h(
    "div",
    {
      className: "contents-table-head",
      onClick: (evt) => evt.stopPropagation(),
      onMouseDown: (evt) => evt.stopPropagation(),
      ref: headerRef,
    },
    [
      h(
        "label",
        { className: "contents-select-all", key: "select-all", title: "Select all visible" },
        h("input", {
          "aria-label": allVisibleSelected
            ? "Deselect all visible items"
            : "Select all visible items",
          checked: allVisibleSelected,
          className: "contents-select-checkbox",
          disabled: visibleCount === 0,
          onChange: onSelectAllChange,
          type: "checkbox",
        })
      ),
      h(
        ContentsHeaderCell,
        {
          columnKey: NAME_COLUMN.key,
          key: NAME_COLUMN.key,
          resizeHandle: COLUMN_RESIZE_HANDLES.name,
          resizeHandlers,
        },
        h(ContentsSortButton, { column: NAME_COLUMN, onSortChange, sort })
      ),
      ...DETAIL_SORT_COLUMNS.map((column) =>
        h(
          ContentsHeaderCell,
          {
            columnKey: column.className,
            key: column.key,
            resizeHandle: COLUMN_RESIZE_HANDLES[column.className],
            resizeHandlers,
          },
          h(ContentsSortButton, { column, onSortChange, sort })
        )
      ),
      h("div", {
        className: "contents-head-actions",
        "data-column-key": "actions",
        key: "actions",
      }),
    ]
  );
}
