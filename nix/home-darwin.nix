self:
{
  config,
  lib,
  pkgs,
  ...
}:
# home-manager module for the macOS local host: runs the daemon under
# launchd and sets up ssh multiplexing for interactive sessions. The
# daemon owns its own persistent SSH master connections (one per host)
# that carry the RemoteForward and serve as control sockets for tunnel
# forwards; the ssh settings here are purely for the user's interactive
# sessions (terminals, herdr, Zed) and do not carry a RemoteForward —
# that would clobber the daemon's master on the remote socket.
let
  cfg = config.programs.porthole;
  home = config.home.homeDirectory;
in
{
  options.programs.porthole = {
    enable = lib.mkEnableOption "the porthole daemon";

    package = lib.mkOption {
      type = lib.types.package;
      # pkgs comes from the module's target — builtins.currentSystem is
      # impure and throws under pure (flake) evaluation.
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.porthole;
      description = "The porthole package to install (full build, daemon included).";
    };

    hosts = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      example = [
        "dev1"
        "launchpad"
      ];
      description = ''
        Remote hosts porthole accepts URLs from. The daemon binds one
        socket per host under ~/.porthole.d/, and spawns one persistent
        SSH master per host carrying the RemoteForward. The names must
        match the Host aliases in your ssh config.
      '';
    };

    zedCommand = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "/Applications/Zed.app/Contents/MacOS/cli";
      description = ''
        Path to the Zed CLI for `zed` requests. When null, the daemon
        probes the usual app-bundle and PATH locations.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    launchd.agents.porthole = {
      enable = true;
      config = {
        ProgramArguments = [
          "${cfg.package}/bin/porthole"
          "daemon"
        ]
        ++ cfg.hosts;
        KeepAlive = true;
        RunAtLoad = true;
        StandardOutPath = "${home}/Library/Logs/porthole.log";
        StandardErrorPath = "${home}/Library/Logs/porthole.log";
      }
      // lib.optionalAttrs (cfg.zedCommand != null) {
        EnvironmentVariables.PORTHOLE_ZED = cfg.zedCommand;
      };
    };

    # SSH multiplexing for interactive sessions (terminals, herdr,
    # Zed). The daemon maintains its own masters on
    # ~/.porthole.d/master/<host>; these settings are for the user's
    # convenience only and do not carry a RemoteForward — the daemon's
    # master owns that, and an interactive-session RemoteForward would
    # clobber it on the remote (StreamLocalBindUnlink replaces the
    # socket on each new connection).
    # programs.ssh.settings — matchBlocks is a deprecated alias.
    programs.ssh.settings = lib.genAttrs cfg.hosts (_host: {
      # Reap half-dead attach sessions fast: a stale forward otherwise
      # accepts client writes into a dead channel, silently.
      ServerAliveInterval = 15;
      ServerAliveCountMax = 2;
      # Multiplex every interactive session to a porthole host over one
      # connection. The user's ControlPath is separate from the
      # daemon's master socket (~/.porthole.d/master/), so the two do
      # not interfere.
      ControlMaster = "auto";
      ControlPath = "${home}/.porthole.d/control/%n";
      ControlPersist = "60m";
    });
  };
}
