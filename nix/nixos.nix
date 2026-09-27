# NixOS: pleamar-wm in the login screen, with pleamar and Marea.
#
#   programs.pleamar-wm.enable = true;
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.pleamar-wm;
  system = pkgs.stdenv.hostPlatform.system;
in
{
  options.programs.pleamar-wm = {
    enable = lib.mkEnableOption "pleamar-wm, a Wayland compositor whose window manager is a pleamar scene";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${system}.pleamar-wm;
      description = "The pleamar-wm package.";
    };
    withMarea = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Marea, the shell its session starts by default.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [
      cfg.package
      self.inputs.pleamar.packages.${system}.pleamar
    ] ++ lib.optional cfg.withMarea self.inputs.marea.packages.${system}.marea;

    # In the login screen's list of sessions.
    services.displayManager.sessionPackages = [ cfg.package ];

    # What a desktop of its own needs: the card, the seat, the portals.
    hardware.graphics.enable = lib.mkDefault true;
    security.polkit.enable = lib.mkDefault true;
    programs.xwayland.enable = lib.mkDefault true;
    xdg.portal = {
      enable = lib.mkDefault true;
      extraPortals = [
        pkgs.xdg-desktop-portal-gtk
        pkgs.xdg-desktop-portal-hyprland
      ];
      configPackages = [ cfg.package ];
    };
  };
}
