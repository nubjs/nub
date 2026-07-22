export type InstallEvent =
  | { kind: "phase"; phase: "resolving" | "fetching" | "linking" | "complete" }
  | {
      kind: "progress"
      resolved: number
      total: number
      reused: number
      downloaded: number
      downloadedBytes: number
      estimatedBytes?: number
    }
  | { kind: "output"; level: "info" | "warning" | "error"; code?: string; message: string }

export interface AubeError extends Error {
  code: string
  diagnostic: string
}

export type ConfigureInput = {
  /**
   * Embedder setting defaults by canonical setting name (e.g.
   * `minimumReleaseAge`). Lowest precedence: environment variables, project
   * files, and user configuration all override them.
   */
  defaults?: Record<string, string>
}
export type InstallInput = {
  add?: { name: string; version?: string; dev?: boolean }[]
  force?: boolean
  offline?: boolean
  osvTransitiveCheck?: boolean
  onEvent?: (event: InstallEvent) => void
  signal?: AbortSignal
}
export type InstallResult = {
  projectDir: string
  added: string[]
  resolved: number
  reused: number
  downloaded: number
  durationMs: number
}
/**
 * Register embedder setting defaults for this process. Call before the first
 * `install`; later (or repeated) calls throw `ERR_AUBE_EMBED_ALREADY_INITIALIZED`,
 * and unknown setting names throw `ERR_AUBE_EMBED_INVALID_SETTING`.
 */
export function configure(input?: ConfigureInput): void
export function install(projectDir: string, input?: InstallInput): Promise<InstallResult>
