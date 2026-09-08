{
  description = "Hedronetes M1: pinned Linux build and development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/c25784012c9982bca5b3e0de87e90bbdac8927d3";
    rust-overlay = {
      url = "github:oxalica/rust-overlay/ca7f624be3935a5bc46d2c240515491ab8675503";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      system = "aarch64-linux";
      pkgs = import nixpkgs { inherit system; overlays = [ rust-overlay.overlays.default ]; };
      rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      rustPlatform = pkgs.makeRustPlatform { cargo = rust; rustc = rust; };
      bun = pkgs.stdenvNoCC.mkDerivation {
        pname = "hedronetes-bun";
        version = "1.4.2";
        src = pkgs.fetchurl {
          url = "https://github.com/oven-sh/bun/releases/download/bun-v1.4.2/bun-linux-aarch64.zip";
          hash = "sha256-VDKLvC2cjgyfiSxUTWbFeoO4QTnjSQnl7oF1jxrI/ac=";
        };
        nativeBuildInputs = [ pkgs.unzip pkgs.autoPatchelfHook ];
        buildInputs = [ pkgs.stdenv.cc.cc.lib ];
        sourceRoot = "bun-linux-aarch64";
        dontBuild = true;
        installPhase = ''
          runHook preInstall
          install -Dm755 bun "$out/bin/bun"
          ln -s bun "$out/bin/bunx"
          runHook postInstall
        '';
        doInstallCheck = true;
        installCheckPhase = ''
          test "$($out/bin/bun --version)" = "1.4.2"
        '';
        meta = { platforms = [ system ]; license = pkgs.lib.licenses.mit; };
      };
      h3s = rustPlatform.buildRustPackage {
        pname = "h3s";
        version = "0.1.0-dev";
        src = pkgs.lib.cleanSource self;
        cargoLock.lockFile = ./Cargo.lock;
        nativeBuildInputs = [ pkgs.cmake pkgs.pkg-config ];
        cargoBuildFlags = [ "--workspace" ];
        cargoTestFlags = [ "--workspace" ];
        meta = { platforms = [ system ]; license = pkgs.lib.licenses.asl20; };
      };
    in {
      packages.${system} = { inherit h3s bun; default = h3s; };
      devShells.${system}.default = pkgs.mkShell {
        packages = [ rust bun pkgs.cmake pkgs.pkg-config pkgs.protobuf
          pkgs.git pkgs.python3 pkgs.nftables pkgs.iproute2 ];
        CARGO_BUILD_JOBS = "4";
      };
    };
}
