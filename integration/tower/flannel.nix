# Pinned upstream Flannel with the nil-annotation panic fixed and tested.
{ lib, buildGoModule, fetchurl, makeWrapper, iptables, nftables }:
buildGoModule {
  pname = "hedronetes-flannel";
  version = "0.28.9-h3s.1";
  src = fetchurl {
    name = "flannel-0.28.9.tar.gz";
    url = "https://codeload.github.com/flannel-io/flannel/tar.gz/refs/tags/v0.28.9";
    hash = "sha256-TL3Y4dS4zKAgkDQNUEcCFdUgqmTMLszqWIZa3ThgvEM=";
  };
  vendorHash = "sha256-MSyjBOznmQNgmzfNauKf344qbE8ixcWMv50CMyY1d84=";
  patches = [ ./patches/flannel-0.28.9-nil-annotations.patch ];
  postPatch = ''
    cp ${./patches/flannel-annotations_test.go} pkg/subnet/kube/h3s_annotations_test.go
  '';
  subPackages = [ "." ];
  env.CGO_ENABLED = "0";
  ldflags = [ "-s" "-w" "-X github.com/flannel-io/flannel/pkg/version.Version=v0.28.9-h3s.1" ];
  nativeBuildInputs = [ makeWrapper ];
  # Kernel/netns tests require privileges; exercise actual networking in the
  # dedicated guests. The Kubernetes subnet manager suite is hermetic here.
  doCheck = true;
  checkPhase = ''
    runHook preCheck
    patch -R -p1 < ${./patches/flannel-0.28.9-nil-annotations.patch}
    if go test -p 2 ./pkg/subnet/kube -run TestAcquireLeaseAnnotations -count=1 > unpatched-test.log 2>&1; then
      cat unpatched-test.log
      echo "unpatched Flannel unexpectedly passed the panic regression" >&2
      exit 1
    fi
    grep -F 'panic: assignment to entry in nil map' unpatched-test.log
    patch -p1 < ${./patches/flannel-0.28.9-nil-annotations.patch}
    go test -p 2 ./pkg/subnet/kube -count=1
    runHook postCheck
  '';
  postInstall = ''
    install -Dm644 LICENSE "$out/share/licenses/flannel/LICENSE"
    install -Dm644 ${./patches/flannel-0.28.9-nil-annotations.patch} "$out/share/hedronetes/flannel-nil-annotations.patch"
    wrapProgram "$out/bin/flannel" --prefix PATH : ${lib.makeBinPath [ iptables nftables ]}
  '';
  doInstallCheck = true;
  installCheckPhase = ''
    "$out/bin/flannel" --version 2>&1 | grep -Fx 'v0.28.9-h3s.1'
  '';
  meta = { platforms = lib.platforms.linux; license = lib.licenses.asl20; mainProgram = "flannel"; };
}
