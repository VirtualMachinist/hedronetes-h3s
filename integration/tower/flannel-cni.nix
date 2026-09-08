{ stdenvNoCC, fetchurl, jq, lib }:
stdenvNoCC.mkDerivation {
  pname = "hedronetes-flannel-cni";
  version = "1.9.1-flannel3";
  src = fetchurl {
    url = "https://github.com/flannel-io/cni-plugin/releases/download/v1.9.1-flannel3/cni-plugin-flannel-linux-arm64-v1.9.1.tgz";
    hash = "sha256-aRwBtaM3XisTkQ73txO8K4E3BH+wn7o2Py8+N5athNk=";
  };
  sourceRoot = ".";
  dontBuild = true;
  nativeInstallCheckInputs = [ jq ];
  installPhase = ''
    runHook preInstall
    install -Dm755 flannel-arm64 "$out/bin/flannel"
    install -Dm644 ${./licenses/flannel-cni-LICENSE} "$out/share/licenses/flannel-cni/LICENSE"
    runHook postInstall
  '';
  doInstallCheck = true;
  installCheckPhase = ''
    CNI_COMMAND=VERSION "$out/bin/flannel" | jq -e '.supportedVersions | index("1.1.0") != null'
  '';
  meta = { platforms = [ "aarch64-linux" ]; license = lib.licenses.asl20; };
}
