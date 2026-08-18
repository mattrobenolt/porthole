self:
{
  config,
  lib,
  pkgs,
  ...
}:
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
    # xdg-utils makes `xdg-open` exist; it reads $BROWSER, which lands
    # on the client. The desktop entry is the fallback route through
    # the MIME database.
    home.packages = [
      cfg.package
      pkgs.xdg-utils
    ];

    # Stock xdg-open and every $BROWSER reader. The package's `open`
    # symlink argv[0]-dispatches to `porthole open`.
    home.sessionVariables.BROWSER = "${cfg.package}/bin/open";

    # mimeApps has its own enable gate (off by default).
    xdg.mimeApps.enable = true;

    # Written directly rather than via xdg.desktopEntries: that option is
    # gated behind the global xdg.enable, which headless configs often
    # disable, and our handler must render regardless. This entry serves
    # gio and other MIME-database consumers; xdg-open's MIME route is
    # display-gated upstream, so headless xdg-open uses $BROWSER.
    home.file.".local/share/applications/porthole.desktop".text = ''
      [Desktop Entry]
      Type=Application
      Name=porthole
      Exec=${cfg.package}/bin/porthole open %u
      NoDisplay=true
    '';

    xdg.mimeApps.defaultApplications = {
      "x-scheme-handler/http" = "porthole.desktop";
      "x-scheme-handler/https" = "porthole.desktop";
    };

    # Link the herdr plugin manifest so Ctrl+click on loopback URLs
    # in herdr panes routes to `porthole open` instead of a dead
    # browser tab. Idempotent: re-linking to the current store path
    # on every switch keeps the registry in sync. Skipped silently
    # when herdr is not installed or its server is not running.
    home.activation.linkPortholeHerdrPlugin =
      lib.hm.dag.entryAfter [ "writeBoundary" ] ''
        if command -v herdr >/dev/null 2>&1; then
          herdr plugin link ${cfg.package}/share/porthole 2>/dev/null || true
        fi
      '';
  };
}
