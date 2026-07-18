import {
  CONTENTS_ICON_PRESETS,
  CONTENTS_VIEW_MODES,
  CONTENTS_VIEW_SLIDER,
  contentsViewAriaValue,
  contentsViewFromSlider,
  contentsViewLabel,
  contentsViewSliderValue,
  normalizeContentsView,
  normalizeWheelDelta,
  sameContentsView,
  stepContentsViewWithWheel,
} from "../../lib/contentsView.js";
import { classNames } from "../../lib/utils.js";
import { Icon } from "../common/Icon.js";

const { useEffect, useRef, useState } = React;
const h = React.createElement;
const SORT_OPTIONS = [
  { key: "name", label: "Name" },
  { key: "modified", label: "Modified" },
  { key: "user", label: "User" },
  { key: "size", label: "Size" },
  { key: "ttl", label: "Status" },
];

function viewIcon(mode) {
  if (mode === CONTENTS_VIEW_MODES.ICONS) {
    return "view-icons";
  }
  return mode === CONTENTS_VIEW_MODES.LIST ? "view-list" : "view-details";
}

function ContentsCompactSort({ onSortChange, sort }) {
  const currentKey = sort?.key || SORT_OPTIONS[0].key;
  return h("div", { className: "contents-compact-sort" }, [
    h("label", { className: "contents-compact-sort-field", key: "field" }, [
      h("span", { className: "visually-hidden", key: "label" }, "Sort contents by"),
      h(
        "select",
        {
          "aria-label": "Sort contents by",
          key: "select",
          onChange: (evt) => onSortChange?.(evt.target.value),
          value: currentKey,
        },
        SORT_OPTIONS.map((option) =>
          h("option", { key: option.key, value: option.key }, option.label)
        )
      ),
    ]),
    h(
      "button",
      {
        "aria-label": sort?.direction === "desc" ? "Sort ascending" : "Sort descending",
        className: "contents-compact-sort-direction",
        key: "direction",
        onClick: () => onSortChange?.(currentKey),
        title: sort?.direction === "desc" ? "Descending" : "Ascending",
        type: "button",
      },
      h(Icon, { icon: sort?.direction === "desc" ? "arrow-down" : "arrow-up", size: 12 })
    ),
  ]);
}

export function ContentsViewToolbarControls({
  allVisibleSelected,
  mode,
  onSelectAllChange,
  onSortChange,
  sort,
  visibleCount,
}) {
  if (mode === CONTENTS_VIEW_MODES.DETAILS) {
    return null;
  }
  return h(React.Fragment, null, [
    h(
      "label",
      {
        className: "contents-compact-select-all",
        key: "select-all",
        title: "Select all visible",
      },
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
    h(ContentsCompactSort, { key: "sort", onSortChange, sort }),
  ]);
}

export function ViewModeControl({ disabled, onChange, view }) {
  const [popoverOpen, setPopoverOpen] = useState(false);
  const [displayView, setDisplayView] = useState(view);
  const rootRef = useRef(null);
  const boundaryDeltaRef = useRef(0);
  const liveViewRef = useRef(view);
  const sliderValue = contentsViewSliderValue(displayView);
  const label = contentsViewLabel(displayView);

  useEffect(() => {
    if (disabled) {
      setPopoverOpen(false);
    }
  }, [disabled]);

  useEffect(() => {
    setDisplayView(view);
    liveViewRef.current = view;
  }, [view]);

  useEffect(() => {
    if (!popoverOpen) {
      return undefined;
    }
    rootRef.current?.querySelector(".view-mode-choice")?.focus();
    function closeOnOutsidePointer(evt) {
      if (!rootRef.current?.contains(evt.target)) {
        setPopoverOpen(false);
      }
    }
    function closeOnEscape(evt) {
      if (evt.key === "Escape") {
        setPopoverOpen(false);
        rootRef.current?.querySelector(".view-mode-trigger")?.focus();
      }
    }
    document.addEventListener("pointerdown", closeOnOutsidePointer);
    document.addEventListener("keydown", closeOnEscape);
    return () => {
      document.removeEventListener("pointerdown", closeOnOutsidePointer);
      document.removeEventListener("keydown", closeOnEscape);
    };
  }, [popoverOpen]);

  function changeView(nextView, options = {}) {
    const normalized = normalizeContentsView(nextView);
    liveViewRef.current = normalized;
    setDisplayView(normalized);
    boundaryDeltaRef.current = 0;
    onChange(normalized, options);
  }

  function handleWheel(evt) {
    if (disabled) {
      return;
    }
    evt.preventDefault();
    evt.stopPropagation();
    const delta = normalizeWheelDelta(evt.deltaY, evt.deltaMode, evt.currentTarget.clientHeight);
    const result = stepContentsViewWithWheel(liveViewRef.current, delta, boundaryDeltaRef.current);
    boundaryDeltaRef.current = result.boundaryDelta;
    if (!sameContentsView(liveViewRef.current, result.view)) {
      liveViewRef.current = result.view;
      setDisplayView(result.view);
      onChange(result.view, { transient: true });
    }
  }

  const choices = [
    ...CONTENTS_ICON_PRESETS.map((preset) => ({
      icon: "view-icons",
      label: preset.label,
      view: { ...displayView, iconSize: preset.size, mode: CONTENTS_VIEW_MODES.ICONS },
    })),
    {
      icon: "view-list",
      label: "List",
      view: { ...displayView, mode: CONTENTS_VIEW_MODES.LIST },
    },
    {
      icon: "view-details",
      label: "Details",
      view: { ...displayView, mode: CONTENTS_VIEW_MODES.DETAILS },
    },
  ];

  return h("div", { className: "view-mode-control", onWheel: handleWheel, ref: rootRef }, [
    popoverOpen
      ? h(
          "div",
          {
            "aria-label": "Contents view",
            className: "view-mode-popover",
            key: "popover",
            role: "dialog",
          },
          [
            h(
              "div",
              { className: "view-mode-choices", key: "choices" },
              choices.map((choice) => {
                const normalizedChoice = normalizeContentsView(choice.view);
                const active =
                  displayView.mode === normalizedChoice.mode &&
                  (displayView.mode !== CONTENTS_VIEW_MODES.ICONS ||
                    displayView.iconSize === normalizedChoice.iconSize);
                return h(
                  "button",
                  {
                    "aria-pressed": active,
                    className: classNames("view-mode-choice", active ? "active" : ""),
                    disabled,
                    key: choice.label,
                    onClick: () => changeView(normalizedChoice),
                    type: "button",
                  },
                  [
                    h(Icon, { icon: choice.icon, key: "icon", size: 14 }),
                    h("span", { key: "label" }, choice.label),
                  ]
                );
              })
            ),
            h("div", { className: "view-mode-slider-column", key: "slider" }, [
              h("input", {
                "aria-label": "Contents view and icon size",
                "aria-orientation": "vertical",
                "aria-valuetext": contentsViewAriaValue(displayView),
                className: "view-mode-slider",
                disabled,
                key: "input",
                max: CONTENTS_VIEW_SLIDER.maximum,
                min: CONTENTS_VIEW_SLIDER.details,
                onChange: (evt) =>
                  changeView(contentsViewFromSlider(evt.target.value, displayView), {
                    transient: true,
                  }),
                onPointerUp: () => onChange(liveViewRef.current, { commit: true }),
                orient: "vertical",
                step: 1,
                type: "range",
                value: sliderValue,
              }),
              h("output", { key: "value" }, `${sliderValue}%`),
            ]),
          ]
        )
      : null,
    h(
      "button",
      {
        "aria-expanded": popoverOpen,
        "aria-haspopup": "dialog",
        "aria-label": `Change contents view. Current view: ${contentsViewAriaValue(displayView)}`,
        className: classNames("view-mode-trigger", popoverOpen ? "active" : ""),
        disabled,
        key: "trigger",
        onClick: () => setPopoverOpen((current) => !current),
        title: "Change view",
        type: "button",
      },
      [
        h(Icon, { icon: viewIcon(displayView.mode), key: "icon", size: 14 }),
        h("span", { className: "view-mode-trigger-label", key: "label" }, label),
        displayView.mode === CONTENTS_VIEW_MODES.ICONS
          ? h("span", { className: "view-mode-trigger-value", key: "value" }, `${sliderValue}%`)
          : null,
        h(Icon, { icon: "chevron-down", key: "chevron", size: 9 }),
      ]
    ),
  ]);
}
