import { mergeFavoriteItems, removeFavoriteItem, sameFavoriteItems } from "./favoriteItems.js";

const { useCallback } = React;

export function useFavoritePreferenceActions({
  closeContextMenu,
  favoriteItems,
  onFavoriteItemsChange,
}) {
  const saveFavoriteItems = useCallback(
    (nextItems) => {
      if (!sameFavoriteItems(nextItems, favoriteItems)) {
        onFavoriteItemsChange(nextItems);
      }
    },
    [favoriteItems, onFavoriteItemsChange]
  );

  const handleAddFavoriteItems = useCallback(
    (items, options = {}) => {
      saveFavoriteItems(mergeFavoriteItems(favoriteItems, items, options));
    },
    [favoriteItems, saveFavoriteItems]
  );

  const handleRemoveFavoriteItem = useCallback(
    (item) => {
      saveFavoriteItems(removeFavoriteItem(favoriteItems, item));
      closeContextMenu();
    },
    [closeContextMenu, favoriteItems, saveFavoriteItems]
  );

  return { handleAddFavoriteItems, handleRemoveFavoriteItem };
}
