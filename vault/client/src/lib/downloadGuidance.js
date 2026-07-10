export async function confirmNativeDownload({ dismissed, onDismiss, requestConfirm }) {
  if (dismissed || !requestConfirm) {
    return true;
  }
  const result = await requestConfirm({
    cancelLabel: "Cancel",
    confirmLabel: "Download",
    message:
      "This browser does not let Vault choose where to save files. To choose a folder each time, enable “Ask where to save each file” in your browser's Downloads settings. That setting applies to this browser profile, not only Vault.",
    rememberLabel: "Do not show again",
    title: "Choose download locations",
  });
  const confirmed = result === true || result?.confirmed === true;
  if (confirmed && result?.remember && onDismiss) {
    await onDismiss();
  }
  return confirmed;
}
