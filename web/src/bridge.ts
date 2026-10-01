import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import type { CommandMap } from "./types";

export class BridgeUnavailableError extends Error {
  readonly code = "desktop-bridge-unavailable";

  constructor() {
    super("The App Manager desktop bridge is unavailable in this browser window.");
    this.name = "BridgeUnavailableError";
  }
}

export function isDesktopRuntime(): boolean {
  if (typeof window === "undefined") return false;
  const candidate = window as Window & {
    __TAURI_INTERNALS__?: unknown;
    __TAURI__?: unknown;
  };
  return Boolean(candidate.__TAURI_INTERNALS__ || candidate.__TAURI__);
}

export function isDemoMode(): boolean {
  if (typeof window !== "undefined") {
    const params = new URLSearchParams(window.location.search);
    if (params.get("demo") === "1") return true;
  }
  return import.meta.env.VITE_DEMO === "1";
}

export async function invokeCommand<K extends keyof CommandMap>(
  command: K,
  args: CommandMap[K]["args"],
): Promise<CommandMap[K]["result"]> {
  if (!isDesktopRuntime()) throw new BridgeUnavailableError();
  const invokeArgs = args === undefined ? undefined : (args as Record<string, unknown>);
  return tauriInvoke<CommandMap[K]["result"]>(command, invokeArgs);
}

export function errorMessage(error: unknown): string {
  if (error instanceof Error && error.message) return error.message;
  if (typeof error === "string" && error) return error;
  return "The desktop command failed without a message.";
}
