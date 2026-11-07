#!/usr/bin/env python3
# Points gir's placeholder dependencies at crates.io and the workspace.
# Usage: fix-sys-manifest.py CRATE_DIR DESCRIPTION
import re
import sys

path = sys.argv[1] + "/Cargo.toml"
s = open(path).read()
s = re.sub(r'(\[dependencies\.(?:glib|gobject|gio)-sys\]\n)git = "[^"]*"',
           r'\1version = "0.22"', s)
s = re.sub(r'(\[dependencies\.camel-sys\]\n)git = "[^"]*"',
           r'\1path = "../camel-sys"', s)
s = re.sub(r'(\[dependencies\.soup3-sys\]\n)git = "[^"]*"',
           r'\1version = "0.9"', s)
s = s.replace('version = "0.0.1"', 'version = "0.1.0"')
if "license =" not in s:
    s = s.replace('build = "build.rs"\n',
                  'build = "build.rs"\nlicense = "LGPL-2.1-or-later"\n'
                  f'description = "{sys.argv[2]}"\n', 1)
open(path, "w").write(s)

# gir requires the newest version in the .gir file; the oldest supported
# one is min_cfg_version in Gir.toml.
gir_toml = open(sys.argv[1] + "/Gir.toml").read()
m = re.search(r'^min_cfg_version = "([^"]+)"', gir_toml, re.M)
if m:
    s = open(path).read()
    s = re.sub(r'(\[package\.metadata\.system-deps\.\w+\]\nname = "[^"]+"\n)version = "[^"]*"',
               lambda x: x.group(1) + f'version = "{m.group(1)}"', s, count=1)
    open(path, "w").write(s)

# gir names the EDataServer crate edata-server-sys.
s = open(path).read()
s = s.replace('name = "edata-server-sys"', 'name = "edataserver-sys"')
s = s.replace('name = "edata_server_sys"', 'name = "edataserver_sys"')
open(path, "w").write(s)
abi = sys.argv[1] + "/tests/abi.rs"
try:
    t = open(abi).read()
    open(abi, "w").write(t.replace("edata_server_sys", "edataserver_sys"))
except FileNotFoundError:
    pass
