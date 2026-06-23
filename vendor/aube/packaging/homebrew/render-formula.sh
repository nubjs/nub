#!/usr/bin/env bash

set -euo pipefail

TAG="${TAG:?TAG is required, e.g. v1.0.0-beta.4}"
SOURCE_SHA256="${SOURCE_SHA256:?SOURCE_SHA256 is required}"

SOURCE_URL="https://github.com/jdx/aube/archive/refs/tags/${TAG}.tar.gz"

cat <<FORMULA
class Aube < Formula
  desc "Fast Node.js package manager that drops into existing projects"
  homepage "https://aube.jdx.dev"
  url "${SOURCE_URL}"
  sha256 "${SOURCE_SHA256}"
  license "MIT"

  depends_on "rust" => :build
  depends_on "usage" => :build
  depends_on "cmake" => :build
  depends_on "pkgconf" => :build

  def install
    system "cargo", "install", *std_cargo_args(path: "crates/aube")
    generate_completions_from_executable(bin/"aube", "completion")
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/aube --version")
    assert_match "Usage:", shell_output("#{bin}/aube --help")
  end
end
FORMULA
