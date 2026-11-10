import { ApiError, api, fromBase, upload } from "@wired-square/appliance-ui";
import type { ApiInit } from "@wired-square/appliance-ui";

import { ARCHIVE_TYPE, reworded } from "./wire";

/** Where this UI passes the gateway's `/v1` on. */
const BASE = "/api/gateway";

function proved(error: unknown): unknown {
  if (!(error instanceof ApiError)) return error;
  const message = reworded(error.status, error.message);
  return message === error.message ? error : new ApiError(error.status, message, error.fromErrorBody);
}

export async function gateway<T>(path: string, init?: ApiInit): Promise<T> {
  try {
    return await api<T>(BASE + path, init);
  } catch (e) {
    throw proved(e);
  }
}

export async function uploadArchive<T>(path: string, file: File, onProgress: (sent: number) => void): Promise<T> {
  try {
    return await upload<T>(BASE + path, file, onProgress, { headers: { "content-type": ARCHIVE_TYPE } });
  } catch (e) {
    throw proved(e);
  }
}

/**
 * Save an export by following its link, so the browser streams the archive to
 * disk; `download` would hold all of it in the page first. The link is the
 * credential, good once.
 */
export function followExport(url: string): void {
  window.location.assign(fromBase(BASE + url));
}
