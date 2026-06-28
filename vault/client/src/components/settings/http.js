export async function responseError(res) {
  try {
    const body = await res.json();
    return body.detail || `Request failed (${res.status})`;
  } catch (_err) {
    return `Request failed (${res.status})`;
  }
}
