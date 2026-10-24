{
  description = "easystream - the Linux est daemon (video/input over est://, HTTP a11y + web UI)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAll (pkgs:
        let
          webui = pkgs.fetchFromGitHub {
            owner = "easydev-org";
            repo = "easystream-webui";
            rev = "ff42ece481464e32a6899ad65d8c1a889cc04ce6";
            hash = "sha256-0ZQwJJVloJNSnllz7oyiQc1tvb+WM3Ob7AABiwQ5SXM=";
          };
        in
        rec {
          easystream = pkgs.rustPlatform.buildRustPackage {
            pname = "easystream";
            version = "0.1.0";
            src = self;
            cargoHash = "sha256-P1aARs8jz4veAUp2+e01M8qJsJrQR84RFvYVAn60+a8=";

            nativeBuildInputs = with pkgs; [ pkg-config ];

            buildInputs = with pkgs; [
              x264 libopus libyuv libxcb
              xorg.libX11 libxkbcommon wayland dbus pipewire
              stdenv.cc.cc.lib
            ];

            PKG_CONFIG_PATH = pkgs.lib.makeSearchPathOutput "dev" "lib/pkgconfig" [
              pkgs.dbus pkgs.pipewire
            ];

            preBuild = ''
              rm -rf webui
              mkdir -p webui
              cp ${webui}/webui/app.js webui/app.js
              cp ${webui}/webui/index.html webui/index.html
            '';

            doCheck = false;

            meta = with pkgs.lib; {
              description = "Linux est daemon: video + input capture/injection";
              platforms = platforms.linux;
              mainProgram = "easystream";
            };
          };
          default = easystream;
        });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          buildInputs = with pkgs; [
            cargo rustc pkg-config
            x264 libopus libyuv libxcb xorg.libX11 libxkbcommon wayland dbus pipewire
          ];
        };
      });
    };
}
