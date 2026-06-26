{
  description = "nub — fast TypeScript-first runtime and pnpm-compatible package manager for Node";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { self, nixpkgs }:
    let
      version = "0.2.3";

      # Per-system prebuilt release tarball + its sha256, verified against the
      # GitHub release assets for v${version}. nub ships prebuilt per-platform
      # tarballs on every channel (curl installer, Homebrew, npm); this flake lays
      # one out rather than building from source. WHY prebuilt, not buildRustPackage:
      # a from-source cargo build omits the vendored runtime/ tree (preload +
      # node_modules + the nub-native.node N-API addon) that nub resolves beside its
      # binary at run time — a bare cargo binary passes `--version` but fails real
      # TypeScript workloads. The prebuilt tarball already carries that layout.
      assets = {
        "x86_64-linux" = {
          file = "nub-linux-x64.tar.gz";
          sha256 = "40f5148ec41988a23bf6d22ed6673a9c0ab69c084eaedaa241ac36a597f883db";
        };
        "aarch64-linux" = {
          file = "nub-linux-arm64.tar.gz";
          sha256 = "9441cc730cb0fdf5acc0762b296f997f2590e0ed1a71ca3ca96a92a34a7a8552";
        };
        "x86_64-darwin" = {
          file = "nub-darwin-x64.tar.gz";
          sha256 = "6f8fa5b1acc5aa047d1447d1e93d05d46acd4146ca332a2489252db5293b300d";
        };
        "aarch64-darwin" = {
          file = "nub-darwin-arm64.tar.gz";
          sha256 = "b550e25cd119db95c9d56fb14cec972956e1d835cea15228c8625cd13cd77f94";
        };
      };

      systems = builtins.attrNames assets;
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);

      nubFor =
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          asset = assets.${system};
        in
        pkgs.stdenv.mkDerivation {
          pname = "nub";
          inherit version;

          src = pkgs.fetchurl {
            url = "https://github.com/nubjs/nub/releases/download/v${version}/${asset.file}";
            sha256 = asset.sha256;
          };

          # The tarball expands to bin/ + runtime/ at the top level (no wrapping dir).
          sourceRoot = ".";

          # The prebuilt Linux binary and nub-native.node link glibc (libc, libm,
          # libgcc_s) and hard-code a /lib64 interpreter — autoPatchelfHook rewrites
          # both for the Nix store. Darwin mach-o binaries need no patching.
          nativeBuildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.autoPatchelfHook ];
          buildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.stdenv.cc.cc.lib ];

          dontConfigure = true;
          dontBuild = true;

          # Lay bin/nub (+ bin/nubx) and the runtime/ sibling out exactly as the
          # tarball ships them. nub canonicalizes current_exe() and walks UP from the
          # binary's directory to find runtime/preload.mjs, so the binary MUST be a
          # real file with runtime/ as a real sibling — no makeWrapper, no symlinked
          # entrypoint, which would canonicalize to a different directory and lose the
          # runtime/ tree.
          installPhase = ''
            runHook preInstall
            mkdir -p "$out"
            cp -r bin "$out/bin"
            cp -r runtime "$out/runtime"
            chmod +x "$out/bin/nub" "$out/bin/nubx"
            runHook postInstall
          '';

          # nub provisions and locates Node itself at run time (from PATH or its own
          # cache), so the package declares no Node runtime dependency here.

          meta = with pkgs.lib; {
            description = "Fast TypeScript-first runtime and pnpm-compatible package manager for Node";
            homepage = "https://nubjs.com";
            downloadPage = "https://github.com/nubjs/nub/releases";
            license = licenses.mit;
            mainProgram = "nub";
            platforms = systems;
            sourceProvenance = [ sourceTypes.binaryNativeCode ];
          };
        };
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
