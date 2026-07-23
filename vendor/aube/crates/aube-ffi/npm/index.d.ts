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

export type AubeResult =
  | { ok: true }
  | { ok: false; code: `ERR_AUBE_${string}`; message: string }

export const libraryPath: string
export const headerPath: string
export const target: string
