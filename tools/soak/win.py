# A window that shows itself for a while and closes: GTK, on Wayland or on X11
# (GDK_BACKEND). Used by churn.sh.
import gi, sys
gi.require_version('Gtk', '3.0')
from gi.repository import Gtk, GLib
w = Gtk.Window(title="soak " + sys.argv[1])
w.add(Gtk.Label(label="soak window " + sys.argv[1] * 40))
w.connect("destroy", Gtk.main_quit)
w.show_all()
GLib.timeout_add(int(float(sys.argv[2]) * 1000), Gtk.main_quit)
Gtk.main()
