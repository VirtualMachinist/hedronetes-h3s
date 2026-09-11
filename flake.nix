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
      youki = rustPlatform.buildRustPackage {
        pname = "hedronetes-youki";
        version = "0.7.0-h3s.1";
        src = pkgs.fetchurl {
          name = "youki-0.7.0.tar.gz";
          url = "https://codeload.github.com/youki-dev/youki/tar.gz/refs/tags/v0.7.0";
          hash = "sha256-9eoB2jvwwx857t6mwPYXb9RjRjFEO8a/xnZUXsEKRhc=";
        };
        cargoLock.lockFile = ./integration/tower/youki-Cargo.lock;
        patches = [ ./integration/tower/patches/youki-0.7.0-exec-seccomp.patch ];
        nativeBuildInputs = [ pkgs.pkg-config pkgs.getconf ];
        buildInputs = [ pkgs.libseccomp pkgs.elfutils pkgs.zlib ];
        nativeCheckInputs = [ pkgs.jq ];
        cargoBuildFlags = [ "-p" "youki" "--features" "v2,systemd,seccomp,cgroupsv2_devices" ];
        cargoTestFlags = [ "-p" "youki" "--features" "v2,systemd,seccomp,cgroupsv2_devices" ];
        postCheck = ''
          cargo test --release --locked --offline -p libcontainer --no-default-features \
            --features v2,systemd,libseccomp,cgroupsv2_devices container::tenant_builder::tests
        '';
        postInstall = ''
          install -Dm644 LICENSE "$out/share/licenses/youki/LICENSE"
          install -Dm644 ${./integration/tower/patches/youki-0.7.0-exec-seccomp.patch} "$out/share/hedronetes/youki-exec-seccomp.patch"
          install -Dm644 ${./integration/tower/patches/README.md} "$out/share/hedronetes/youki-patches.md"
        '';
        doInstallCheck = true;
        installCheckPhase = ''
          "$out/bin/youki" --version
          "$out/bin/youki" features | jq -e '.linux.cgroup.v2 == true and .linux.cgroup.systemd == true'
          "$out/bin/youki" --version | jq -R -s -e 'test("(?m)^libseccomp: [0-9]+[.][0-9]+[.][0-9]+$")'
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
      flannel = pkgs.callPackage ./integration/tower/flannel.nix {};
      flannel-cni = pkgs.callPackage ./integration/tower/flannel-cni.nix {};
      runtime-tools = pkgs.buildEnv {
        name = "hedronetes-runtime-tools";
        paths = [ containerd youki pkgs.iproute2 pkgs.nftables
          pkgs.util-linux pkgs.kmod pkgs.bash pkgs.coreutils pkgs.jq ];
        pathsToLink = [ "/bin" "/share/licenses" ];
        postBuild = ''
          mkdir -p "$out/share/hedronetes"
          ln -s ${cni-plugins}/bin "$out/share/hedronetes/cni-bin"
          ln -s ${youki}/share/hedronetes "$out/share/hedronetes/youki"
        '';
      };
      h3s = rustPlatform.buildRustPackage {
        pname = "h3s";
        version = "0.9.1";
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
      packages.${system} = { inherit h3s bun youki containerd cni-plugins runtime-tools flannel flannel-cni; default = h3s; };
      nixosModules.h3s = import ./integration/tower/nixos/h3s.nix;
      devShells.${system}.default = pkgs.mkShell {
        packages = [ rust bun pkgs.cmake pkgs.pkg-config pkgs.protobuf
          pkgs.git pkgs.python3 pkgs.openssl pkgs.nftables pkgs.iproute2 ];
        CARGO_BUILD_JOBS = "4";
      };
    };
}
