{
  description = "A fast all-in-one toolkit that augments Node.js instead of replacing it";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/24.11";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
        };
        
        # Build the nub CLI using naersk for faster development iteration
        nub-cli = pkgs.rustPlatform.buildRustPackage {
          pname = "nub";
          version = "0.1.14";
          
          src = ./.;
          
          cargoLock = {
            lockFile = ./Cargo.lock;
          };
          
          nativeBuildInputs = with pkgs; [
            pkg-config
          ];
          
          buildInputs = with pkgs; [
          ] ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
            pkgs.darwin.apple_sdk.frameworks.Security
            pkgs.darwin.apple_sdk.frameworks.CoreFoundation
          ];
          
          cargoBuildFlags = [ "--release" ];
          doCheck = false;  # Skip tests for faster iteration
          
          meta = with pkgs.lib; {
            description = "A fast all-in-one toolkit that augments Node.js instead of replacing it";
            homepage = "https://github.com/nubjs/nub";
            license = licenses.mit;
            maintainers = [ ];
            mainProgram = "nub";
          };
        };
      in
      {
        packages.default = nub-cli;
        
        apps.default = flake-utils.lib.mkApp {
          drv = nub-cli;
          exePath = "/bin/nub";
        };
        
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rustc
            cargo
            pkg-config
          ] ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
            darwin.apple_sdk.frameworks.Security
            darwin.apple_sdk.frameworks.CoreFoundation
          ];
          
          shellHook = ''
            echo "Nub development environment"
            echo "Rust version: $(rustc --version)"
            echo "Cargo version: $(cargo --version)"
          '';
        };
      }
    );
}