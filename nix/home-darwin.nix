self:
{
  config,
  lib,
  pkgs,
  ...
}:
# home-manager module for the macOS local host: runs the daemon under
# launchd and declares one ssh RemoteForward per managed host, from the
# same attrset — one edit wires both halves of the contract.
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
        socket per host under ~/.porthole.d/, and one ssh matchBlock
        RemoteForward is generated per host. The names must match the
        Host aliases in your ssh config.
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
      };
    };

    # One RemoteForward per host: the remote's ~/.porthole.sock lands on
    # the daemon's per-host socket here. Address-only entries render as
    # unix socket paths (home-manager asserts paths carry no port).
    # programs.ssh.settings — matchBlocks is a deprecated alias.
    programs.ssh.settings = lib.genAttrs cfg.hosts (host: {
      # Reap half-dead attach sessions fast: a stale forward otherwise
      # accepts client writes into a dead channel, silently.
      ServerAliveInterval = 15;
      ServerAliveCountMax = 2;
      RemoteForward = [
        {
          bind.address = "${home}/.porthole.sock";
          host.address = "${home}/.porthole.d/${host}.sock";
        }
      ];
    });
  };
}
