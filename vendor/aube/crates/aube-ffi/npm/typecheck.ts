import {
  headerPath,
  libraryPath,
  target,
  type AubeResult,
  type InstallEvent,
} from "@jdxcode/aube-ffi"

function handleEvent(event: InstallEvent) {
  switch (event.kind) {
    case "phase":
      return event.phase
    case "progress":
      return event.resolved + event.downloadedBytes
    case "output":
      return `${event.level}:${event.message}`
    default: {
      const exhaustive: never = event
      return exhaustive
    }
  }
}

const paths: string[] = [libraryPath, headerPath, target]
const result: AubeResult = { ok: true }
void [paths, result, handleEvent]
