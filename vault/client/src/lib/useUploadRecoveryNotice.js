import { discoverRecoverableUploads, recoverableUploadNotice } from "./uploadRecovery.js";

const { useEffect } = React;

export function useUploadRecoveryNotice({ apiFetch, showNotice }) {
  useEffect(() => {
    const controller = new AbortController();
    discoverRecoverableUploads({ apiFetch, signal: controller.signal })
      .then((recoveries) => {
        if (controller.signal.aborted) {
          return;
        }
        const recoveryNotice = recoverableUploadNotice(recoveries);
        if (recoveryNotice) {
          showNotice(recoveryNotice);
        }
      })
      .catch(() => {
        // Startup discovery is advisory; normal upload selection will perform
        // the authoritative resume negotiation again.
      });
    return () => controller.abort();
  }, [apiFetch, showNotice]);
}
