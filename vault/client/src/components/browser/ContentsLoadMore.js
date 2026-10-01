const h = React.createElement;

export function ContentsLoadMore({ hasMore, loading, onLoadMore }) {
  if (!hasMore && !loading) {
    return null;
  }
  return h(
    "div",
    {
      "aria-live": "polite",
      className: "contents-load-more",
      onClick: (e) => e.stopPropagation(),
      onMouseDown: (e) => e.stopPropagation(),
    },
    h(
      "button",
      {
        "aria-busy": loading ? "true" : undefined,
        "aria-label": loading ? "Loading more contents" : "Load more contents",
        className: "btn secondary contents-load-more-button",
        disabled: loading,
        onClick: onLoadMore,
        type: "button",
      },
      loading ? "Loading more…" : "Load more"
    )
  );
}
