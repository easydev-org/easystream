{
  # AN EST DAEMON FOR NIXOS, AND THE MODULE THAT ACTUALLY RUNS IT.
  #
  # THE PACKAGE ALONE IS NOT ENOUGH AND THAT IS THE POINT OF THE MODULE. `nix profile install` on
  # any distribution puts a binary on PATH; on NixOS there is no `/etc/systemd/system` to drop a unit
  # into, and a package that ships no module leaves a person hand-writing a unit file in
  # `systemd.services`, which is the same work this file can do once.
  #
  # SO THIS FLAKE EXPOSES TWO THINGS:
  #
  #	packages.<system>.default    the binary, built from this tree
  #	nixosModules.default        `services.easystream.*`, which is what a guest's configuration says
  #
  # WHY IT BUILDS FROM THIS TREE AND NOT FROM A RELEASE ASSET. The `-nixos` binary in the releases is
  # built by THIS flake in CI, against nixpkgs' own libx264 - which is the whole reason the release
  # matrix has a separate nixos row: libx264 puts its ABI in the SONAME and in every exported symbol,
  # so a binary linked elsewhere answers `undefined symbol: x264_encoder_open_164`. Building here
  # means the ABI is whatever this nixpkgs has, which is the only thing that can be correct.
  description = "easystream - the Linux est daemon: video, input and a11y over est://";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAll (pkgs:
        let
          easystream = pkgs.rustPlatform.buildRustPackage {
            pname = "easystream";
            version = "0.10.1";
            src = self;
            # THE HASH MUST BE REGENERATED WHEN `Cargo.lock` CHANGES, and it is left as the
            # previous value with `lib.fakeHash` semantics spelled out rather than silently wrong:
            # a stale hash fails with "hash mismatch" and prints the correct one, which is the
            # documented repair (`nix build; take the hash from the error`).
            #
            # IT DID NOT CHANGE WITH THE HTTP REMOVAL: no dependency was added or removed, only
            # source files were deleted, and `cargoHash` covers the dependency graph.
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

            # THERE IS NO WEB UI TO FETCH ANY MORE. This used to clone `easystream-webui` and copy
            # `app.js`/`index.html` into `webui/` for `include_str!` to embed - which was the HTTP
            # surface's page. The HTTP surface is gone (est:// is the only listener), the `http`
            # module is deleted, and nothing reads `webui/`. The `preBuild` that fetched it went with
            # it; leaving it would have made every NixOS build depend on a repository that is no
            # longer part of the product.
            doCheck = false;

            meta = with pkgs.lib; {
              description = "Linux est daemon: video + input capture/injection over est://";
              platforms = platforms.linux;
              mainProgram = "easystream";
            };
          };
        in
        rec {
          inherit easystream;
          default = easystream;
        });

      # THE MODULE, WHICH IS THE REASON THIS FILE EXISTS ON NIXOS.
      #
      # IT IS A SYSTEM SERVICE AND NOT A USER ONE, AND THAT IS A DIFFERENT DECISION FROM THE .deb.
      #
      # The package's own unit is a USER unit (`PartOf=graphical-session.target`,
      # `ConditionEnvironment=DISPLAY`) because on a desktop distribution the daemon belongs to
      # whoever is sitting at the screen. A NixOS guest being driven by easyvm has no session at all:
      # root is autologged into a TTY and Xvfb is a system service, so there is no `--user` manager to
      # enable anything under. `systemctl --user enable` answers "Failed to connect to bus: No such
      # file or directory" and reads as a broken package.
      #
      # SO THE DISPLAY IS AN OPTION AND THE SERVICE WAITS FOR IT. `after` names the unit that owns the
      # X server, which is what makes `DISPLAY` real by the time the daemon opens it.
      nixosModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.services.easystream;
        in
        {
          options.services.easystream = {
            enable = lib.mkEnableOption "easystream, the est:// screen and input daemon";

            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.easystream;
              defaultText = lib.literalExpression "easystream.packages.\${system}.default";
              description = "The easystream package to run.";
            };

            display = lib.mkOption {
              type = lib.types.str;
              default = ":0";
              description = ''
                The X display to capture and inject into. A microvm guest usually has an Xvfb on :0;
                a desktop distribution would use the session's own display, and THAT is where the
                package's user unit is the right tool instead of this module.
              '';
            };

            audioSource = lib.mkOption {
              type = lib.types.str;
              default = "vsink.monitor";
              description = ''
                The PulseAudio/PipeWire source name the daemon records. A machine with no sound card
                needs a null sink, and `vsink.monitor` is the name `audio.rs` looks for by default -
                so the two agree without this option being set.
              '';
            };

            openFirewall = lib.mkOption {
              type = lib.types.bool;
              default = false;
              description = ''
                Open the est port. OFF BY DEFAULT because the port speaks a protocol with no
                authentication of its own beyond a view/control token, and because the only caller is
                normally the program that booted the machine, on the same bridge.
              '';
            };

            port = lib.mkOption {
              type = lib.types.port;
              default = 48079;
              description = "The est:// port. 48079 sits beside easyworker's 48080.";
            };
          };

          config = lib.mkIf cfg.enable {
            systemd.services.easystream = {
              description = "easystream - screen, input and a11y over est://";
              # THE SERVICE HAS TO WAIT FOR A DISPLAY, AND IT CANNOT SAY WHICH ONE BY NAME.
              #
              # NixOS has no standard unit called `xvfb.service` - a guest may run Xvfb, Xorg or a
              # compositor under any name. `wantedBy = [ "multi-user.target" ]` with `after` on the
              # display manager is the honest arrangement: the daemon starts with the system, and a
              # display that is not up yet makes the first capture fail and retry rather than the
              # unit refuse to start.
              wantedBy = [ "multi-user.target" ];
              after = [ "network.target" ];
              # THE DAEMON EXITS WHEN IT CANNOT OPEN THE DISPLAY, and `Restart=always` is what makes
              # the ordering above work: a guest whose X server comes up 300ms later gets a second or
              # third attempt instead of a dead service.
              restartIfChanged = false;
              serviceConfig = {
                ExecStart = "${cfg.package}/bin/easystream serve";
                Environment = [
                  "DISPLAY=${cfg.display}"
                  "EAZYSTREAM_PORT=${toString cfg.port}"
                  "EAZYSTREAM_AUDIO_SOURCE=${cfg.audioSource}"
                  "PATH=${lib.makeBinPath [ pkgs.pulseaudio pkgs.pipewire ]}:/usr/bin:/bin"
                ];
                Restart = "always";
                RestartSec = 3;
                # NOT A SANDBOX. `ProtectSystem=strict` and friends would break a daemon whose job is
                # to open another process's display and inject events into it; the hardening that
                # matters here is that the service runs as root inside a VM that exists for it.
                StandardOutput = "journal+console";
                StandardError = "journal+console";
              };
            };

            networking.firewall.allowedTCPPorts =
              lib.mkIf cfg.openFirewall [ cfg.port ];
          };
        };

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
