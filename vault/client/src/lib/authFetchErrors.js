export function normalizedAuthFetchError(error) {
  if (error?.status === 401 || error?.name === "AbortError") {
    return error;
  }
  const connectionError = new Error("Lost connection to the server.");
  connectionError.status = 0;
  return connectionError;
}
