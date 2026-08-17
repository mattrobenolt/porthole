self:
{ config, lib, pkgs, ... }:
# home-manager module for a NixOS remote: installs the porthole client
# and registers it as the machine's "browser" so xdg-open, $BROWSER,
# gio/mimeapps, and macOS-style `open` calls all route to the mac.
let
  cfg = config.programs.porthole;
in
{
  options.programs.porthole = {
    enable = lib.mkEnableOption "porthole remote URL forwarding";

    package = lib.mkOption {
      type = lib.types.package;
      # pkgs comes from the module's target — builtins.currentSystem is
      # impure and throws under pure (flake) evaluation.
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.porthole-remote;
      description = "The porthole package to install. Use the -remote build (client only, no tokio).";
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    # Stock xdg-open and every $BROWSER reader. The package's `open`
    # symlink argv[0]-dispatches to `porthole open`.
    home.sessionVariables.BROWSER = "${cfg.package}/bin/open";

    xdg.desktopEntries.porthole = {
      name = "porthole";
      exec = "${cfg.package}/bin/porthole open %u";
      noDisplay = true;
    };

    xdg.mimeApps.defaultApplications = {
      "x-scheme-handler/http" = "porthole.desktop";
      "x-scheme-handler/https" = "porthole.desktop";
    };
  };
}
