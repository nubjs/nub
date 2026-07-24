{
  description = "nub — fast TypeScript-first runtime and pnpm-compatible package manager for Node";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { self, nixpkgs }:
    let
      # Track the workspace version automatically — the binary's own version comes
      # from Cargo.toml (CARGO_PKG_VERSION) at build time, so this attr is only
      # package metadata; read it from the same source to avoid a manual sync point.
      version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;

      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);

      nubFor =
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          inherit (pkgs) lib rustPlatform;

          # ── Embedded-runtime node_modules ──────────────────────────────────
          # The single-binary build embeds the `runtime/` tree (preload + native
          # addon + a small vendored node_modules) INTO the binary; build.rs
          # tars+zstd-compresses it and `include_bytes!`s the blob, JIT-extracting
          # it on first run. So the Nix sandbox must assemble the same `runtime/`
          # tree BEFORE the cargo build — including these pure-JS runtime deps that
          # the EMITTED transpile output / web-API polyfills require (oxc itself is
          # compiled into the addon, not vendored here). Versions + integrity are
          # pinned to the workspace pnpm-lock.yaml (the sha512 SRI is the lockfile's
          # own `integrity` field, accepted verbatim by fetchurl), so a shipped
          # nub's transpile output is byte-identical to dev's. `jsbi` is the one
          # transitive dep (of @js-temporal/polyfill).
          npmTarball =
            {
              name,
              file,
              hash,
            }:
            pkgs.fetchurl {
              url = "https://registry.npmjs.org/${name}/-/${file}";
              inherit hash;
            };

          runtimeDeps = [
            {
              dest = "@js-temporal/polyfill";
              src = npmTarball {
                name = "@js-temporal/polyfill";
                file = "polyfill-0.5.1.tgz";
                hash = "sha512-hloP58zRVCRSpgDxmqCWJNlizAlUgJFqG2ypq79DCvyv9tHjRYMDOcPFjzfl/A1/YxDvRCZz8wvZvmapQnKwFQ==";
              };
            }
            {
              dest = "@oxc-project/runtime";
              src = npmTarball {
                name = "@oxc-project/runtime";
                file = "runtime-0.140.0.tgz";
                hash = "sha512-ZYZwESARwc2fB/vLHj+991uu3iGIsor3gPTA7wgouM6xEJzJsz11dt+/Xl5tUinERqmq6auyRhSMWzChhfez6w==";
              };
            }
            {
              dest = "@petamoriken/float16";
              src = npmTarball {
                name = "@petamoriken/float16";
                file = "float16-3.9.3.tgz";
                hash = "sha512-8awtpHXCx/bNpFt4mt2xdkgtgVvKqty8VbjHI/WWWQuEw+KLzFot3f4+LkQY9YmOtq7A5GdOnqoIC8Pdygjk2g==";
              };
            }
            {
              dest = "urlpattern-polyfill";
              src = npmTarball {
                name = "urlpattern-polyfill";
                file = "urlpattern-polyfill-10.1.0.tgz";
                hash = "sha512-IGjKp/o0NL3Bso1PymYURCJxMPNAf/ILOpendP9f5B6e1rTJgdgiOvgfoT8VxCAdY+Wisb9uhGaJJf3yZ2V9nw==";
              };
            }
            {
              dest = "jsbi";
              src = npmTarball {
                name = "jsbi";
                file = "jsbi-4.3.2.tgz";
                hash = "sha512-9fqMSQbhJykSeii05nxKl4m6Eqn2P6rOlYiS+C5Dr/HPIU/7yZxu5qzbs40tgaFORiw2Amd0mirjxatXYMkIew==";
              };
            }
          ];

          # Lay the fetched tarballs out as a real `node_modules/` tree (each npm
          # tarball expands to `package/`; --strip-components=1 drops that wrapper).
          # Real file copies, not symlinks, so the tar embedded into the binary is
          # fully self-contained — no /nix/store symlinks survive into ~/.cache/nub.
          runtimeNodeModules = pkgs.runCommand "nub-runtime-node-modules-${version}" { } (
            ''
              mkdir -p "$out"
            ''
            + lib.concatMapStringsSep "\n" (dep: ''
              mkdir -p "$out/${dep.dest}"
              tar -xzf "${dep.src}" --strip-components=1 -C "$out/${dep.dest}"
            '') runtimeDeps
          );

          # ── Native N-API addon (separate cargo workspace) ──────────────────
          # crates/nub-native is its OWN cargo workspace (it keeps panic=unwind for
          # the cdylib while the `nub` workspace uses panic=abort) with its OWN
          # Cargo.lock, so it is NOT built by the main `nub` build and needs its own
          # derivation. It cross-depends on crates/nub-cache-key by path, so it must
          # build from the full source tree (src = self), not an isolated copy. The
          # output is the `.node` the embedded runtime resolves as
          # `addons/nub-native.node`.
          nubNativeAddon = rustPlatform.buildRustPackage {
            pname = "nub-native";
            inherit version;
            src = self;
            # crates/nub-native is a SELF-CONTAINED cargo workspace in a subdir (its
            # own Cargo.toml + Cargo.lock) living inside the larger repo, so it needs
            # BOTH knobs, honored by different hooks:
            #   - `cargoRoot` points the vendoring + the lock-consistency check at
            #     crates/nub-native/Cargo.lock (without it, the check validates the
            #     ROOT workspace lock against the addon's vendor dir and fails).
            #   - `buildAndTestSubdir` cds the BUILD into crates/nub-native so cargo
            #     resolves the nub-native workspace. Without it the build runs at the
            #     source root and resolves the ROOT workspace (nub-cli) against the
            #     addon vendor dir — which lacks nub-cli-only crates like anyhow.
            # The cross-workspace `../nub-cache-key` path dep resolves either way —
            # src = self carries the whole tree.
            cargoRoot = "crates/nub-native";
            buildAndTestSubdir = "crates/nub-native";
            cargoLock.lockFile = ./crates/nub-native/Cargo.lock;
            doCheck = false;

            # Install the cdylib as the addon. The crate's .cargo/config.toml routes
            # output into the repo-root target/, and nixpkgs builds under a target
            # triple, so search the whole build tree (cwd is crates/nub-native, but
            # the artifact lands at <source>/target/<triple>/release/) by exact name
            # — which skips the hash-suffixed deps/ copy.
            installPhase = ''
              runHook preInstall
              mkdir -p "$out/lib"
              addon=$(find "$NIX_BUILD_TOP" -type f \
                \( -name 'libnub_native.so' -o -name 'libnub_native.dylib' \) \
                -path '*/release/*' | head -1)
              if [ -z "$addon" ]; then
                echo "nub-native: built addon dylib not found under target/*/release" >&2
                exit 1
              fi
              cp "$addon" "$out/lib/nub-native.node"
              runHook postInstall
            '';

            meta.description = "nub N-API transpiler addon (embedded into the nub binary)";
          };

          # ── The nub binary (root workspace, single self-contained build) ───
          nub = rustPlatform.buildRustPackage {
            pname = "nub";
            inherit version;
            src = self;

            cargoLock.lockFile = ./Cargo.lock;
            buildAndTestSubdir = "crates/nub-cli";

            # The hermetic Nix sandbox ships no system tools, so the C-building
            # crates in nub's graph need their build tools declared explicitly:
            # aws-lc-sys (via aube → sigstore) and libz-ng-sys (via flate2's
            # zlib-ng feature) both compile through cmake. Every existing CI runner
            # ships cmake preinstalled, which is why this surfaces only under Nix.
            nativeBuildInputs = [
              pkgs.cmake
              pkgs.perl
            ];

            # Single self-contained binary: embed the staged runtime/ tree. nub-core's
            # build.rs (gated on this feature) tars+zstd-19s `<repo>/runtime` and
            # `include_bytes!`s it; ruzstd decodes at runtime, extracting once to
            # ~/.cache/nub. This is the whole reason the flake can now build FROM
            # SOURCE: a feature-off cargo build produces a binary that walks to a
            # runtime/ SIDECAR (absent here), which is why the flake formerly shipped
            # a prebuilt tarball. The embedded runtime dissolves that — no sidecar.
            buildFeatures = [ "embed-runtime" ];

            # Tests aren't part of the package build (they need Node + a network
            # registry); the upstream CI matrix owns them.
            doCheck = false;

            # Stage runtime/ BEFORE the build so build.rs embeds a COMPLETE tree.
            # The tracked runtime/*.{mjs,cjs} ship in the source; add the per-platform
            # addon + the vendored node_modules. build.rs panics on a missing
            # preload.mjs and embeds whatever is staged, so this must run before
            # configure/build.
            postPatch = ''
              chmod -R u+w runtime
              mkdir -p runtime/addons
              cp "${nubNativeAddon}/lib/nub-native.node" runtime/addons/nub-native.node
              cp -rL "${runtimeNodeModules}" runtime/node_modules
              chmod -R u+w runtime
            '';

            # nub dispatches its nub/nubx personality from the argv[0] basename
            # (cli.rs Argv0::detect → file_stem), NOT from a canonicalized
            # current_exe(), so a symlink invoked as `nubx` enters nubx mode — no
            # need for a second real binary copy.
            postInstall = ''
              ln -s nub "$out/bin/nubx"
            '';

            # The Node N-API addon is embedded as opaque bytes, so Nix can't see its
            # store references — but it links the SAME nixpkgs glibc/libgcc as this
            # binary, which IS in this package's closure (referenced by the binary's
            # own interpreter/rpath), so the libs the runtime-extracted addon needs
            # at dlopen stay live. nub provisions/locates Node itself at run time, so
            # no Node runtime dependency is declared.

            meta = with lib; {
              description = "Fast TypeScript-first runtime and pnpm-compatible package manager for Node";
              homepage = "https://nubjs.com";
              downloadPage = "https://github.com/nubjs/nub/releases";
              license = licenses.mit;
              mainProgram = "nub";
              platforms = systems;
            };
          };
        in
        nub;
    in
    {
      packages = forAllSystems (
        system: rec {
          nub = nubFor system;
          default = nub;
        }
      );

      apps = forAllSystems (
        system:
        let
          nubPkg = nubFor system;
        in
        {
          nub = {
            type = "app";
            program = "${nubPkg}/bin/nub";
          };
          nubx = {
            type = "app";
            program = "${nubPkg}/bin/nubx";
          };
          default = {
            type = "app";
            program = "${nubPkg}/bin/nub";
          };
        }
      );
    };
}
