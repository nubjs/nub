function platformPackage(target) {
  const { os, arch } = target
  const supported = (os === "darwin" || os === "win32" || os === "linux") && (arch === "arm64" || arch === "x64")
  if (!supported) throw new Error(`Unsupported aube Node-API build target: ${os}-${arch}`)
  const libc = os === "linux" && target.libc === "musl" ? "-musl" : ""
  return `@jdxcode/aube-node-${os}-${arch}${libc}`
}

function bunPlugin(target) {
  const packageName = platformPackage(target)
  const addon = require.resolve(packageName)
  return {
    name: "aube-node-platform",
    setup(build) {
      build.onResolve({ filter: /^@jdxcode\/aube-node$/ }, () => ({
        path: "aube-node-platform",
        namespace: "aube-node",
      }))
      build.onLoad({ filter: /.*/, namespace: "aube-node" }, () => ({
        contents: [
          `const addon = require(${JSON.stringify(addon)})`,
          "export const install = addon.install",
          "export default addon",
        ].join("\n"),
        loader: "js",
      }))
    },
  }
}

module.exports = { bunPlugin, platformPackage }
