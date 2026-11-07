#!/bin/sh
# Copies EDS .gir files from the system, patches them and regenerates the
# binding crates. GIR overrides the gir tool binary path.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
GIR=${GIR:-gir}
SYSGIR=${SYSGIR:-/usr/share/gir-1.0}
for f in Camel-1.2 EDataServer-1.2 Soup-3.0 Json-1.0; do
	cp "$SYSGIR/$f.gir" "$root/gir-files/"
done
python3 -I "$root/scripts/fix-girs.py" "$root"/gir-files/Camel-1.2.gir \
	"$root"/gir-files/EDataServer-1.2.gir
for c in camel-sys camel edataserver-sys edataserver; do
	[ -f "$root/bindings/$c/Gir.toml" ] || continue
	(cd "$root/bindings/$c" && "$GIR" -c Gir.toml)
	case $c in *-sys) python3 -I "$root/scripts/fix-sys-manifest.py" "$root/bindings/$c" "FFI bindings to the ${c%-sys} library of evolution-data-server" ;; esac
done
