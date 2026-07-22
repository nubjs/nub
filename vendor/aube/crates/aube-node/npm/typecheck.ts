import { configure, install, type AubeError, type InstallEvent } from "@jdxcode/aube-node"

function handleEvent(event: InstallEvent) {
  switch (event.kind) {
    case "phase":
      return event.phase
    case "progress":
      return event.resolved + event.reused + event.downloaded
    case "output":
      return `${event.level}:${event.message}`
    default: {
      const exhaustive: never = event
      return exhaustive
    }
  }
}

configure({ defaults: { minimumReleaseAge: "259200" } })

const controller = new AbortController()
install(".", {
  osvTransitiveCheck: true,
  add: [{ name: "typescript", version: "latest", dev: true }],
  signal: controller.signal,
  onEvent: handleEvent,
}).then((result) => result.durationMs)

const error = new Error("example") as AubeError
error.code.toUpperCase()
error.diagnostic.toLowerCase()
