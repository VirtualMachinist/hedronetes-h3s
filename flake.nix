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
      youki = pkgs.stdenvNoCC.mkDerivation {
        pname = "hedronetes-youki";
        version = "0.7.0";
        src = pkgs.fetchurl {
          url = "https://github.com/youki-dev/youki/releases/download/v0.7.0/youki-0.7.0-aarch64-musl.tar.gz";
          hash = "sha256-uWwFwsgvHSCnS2ERiPoSCJTFCmEo9zhWuzcWBOy2m9A=";
        };
        sourceRoot = ".";
        dontBuild = true;
        installPhase = ''
          runHook preInstall
          install -Dm755 youki "$out/bin/youki"
          install -Dm644 LICENSE "$out/share/licenses/youki/LICENSE"
          runHook postInstall
        '';
        doInstallCheck = true;
        installCheckPhase = ''
          "$out/bin/youki" --version
        '';
        meta = { platforms = [ system ]; license = pkgs.lib.licenses.asl20; };
      };
      containerd = pkgs.stdenvNoCC.mkDerivation {
        pname = "hedronetes-containerd";
        version = "2.3.5";
        src = pkgs.fetchurl {
          url = "https://github.com/containerd/containerd/releases/download/v2.3.5/containerd-static-2.3.5-linux-arm64.tar.gz";
          hash = "sha256-AmzW4o9g2pzrvozwxgKpNcaqSKoldJX+xHjQIfcbT/U=";
        };
        sourceRoot = ".";
        dontBuild = true;
        installPhase = ''
          runHook preInstall
          mkdir -p "$out/bin"
          cp bin/* "$out/bin/"
          install -Dm644 ${./integration/tower/licenses/containerd-LICENSE} "$out/share/licenses/containerd/LICENSE"
          runHook postInstall
        '';
        doInstallCheck = true;
        installCheckPhase = ''
          "$out/bin/containerd" --version
        '';
        meta = { platforms = [ system ]; license = pkgs.lib.licenses.asl20; };
      };
      cni-plugins = pkgs.stdenvNoCC.mkDerivation {
        pname = "hedronetes-cni-plugins";
        version = "1.9.1";
        src = pkgs.fetchurl {
          url = "https://github.com/containernetworking/plugins/releases/download/v1.9.1/cni-plugins-linux-arm64-v1.9.1.tgz";
          hash = "sha256-VhcZh9OUdwfDVj2y9AAbzK9Q/WNGhhG588vssTde5+w=";
        };
        sourceRoot = ".";
        dontBuild = true;
        installPhase = ''
          runHook preInstall
          mkdir -p "$out/bin"
          for plugin in ./*; do
            if [ -f "$plugin" ] && [ -x "$plugin" ]; then
              install -m755 "$plugin" "$out/bin/"
            fi
          done
          install -Dm644 LICENSE "$out/share/licenses/cni-plugins/LICENSE"
          runHook postInstall
        '';
        doInstallCheck = true;
        installCheckPhase = ''
          CNI_COMMAND=VERSION "$out/bin/bridge"
        '';
        meta = { platforms = [ system ]; license = pkgs.lib.licenses.asl20; };
      };
      runtime-tools = pkgs.buildEnv {
        name = "hedronetes-runtime-tools";
        paths = [ containerd youki pkgs.iproute2 pkgs.nftables
          pkgs.util-linux pkgs.kmod pkgs.bash pkgs.coreutils ];
        postBuild = ''
          mkdir -p "$out/share/hedronetes"
          ln -s ${cni-plugins}/bin "$out/share/hedronetes/cni-bin"
        '';
      };
      h3s = rustPlatform.buildRustPackage {
        pname = "h3s";
        version = "0.1.0-dev";
        src = pkgs.lib.cleanSourceWith {
          src = self;
          filter = path: type:
            let name = baseNameOf path; in
            pkgs.lib.cleanSourceFilter path type
              && name != "target" && !(pkgs.lib.hasPrefix "._" name);
        };
        cargoLock.lockFile = ./Cargo.lock;
        nativeCheckInputs = [ pkgs.openssl ];
        nativeBuildInputs = [ pkgs.cmake pkgs.pkg-config ];
        cargoBuildFlags = [ "--workspace" ];
        cargoTestFlags = [ "--workspace" ];
        meta = { platforms = [ system ]; license = pkgs.lib.licenses.asl20; };
      };
    in {
      packages.${system} = { inherit h3s bun youki containerd cni-plugins runtime-tools; default = h3s; };
      devShells.${system}.default = pkgs.mkShell {
        packages = [ rust bun pkgs.cmake pkgs.pkg-config pkgs.protobuf
          pkgs.git pkgs.python3 pkgs.openssl pkgs.nftables pkgs.iproute2 ];
        CARGO_BUILD_JOBS = "4";
      };
    };
}
