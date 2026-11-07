#!/usr/bin/env python3
# Patches EDS .gir files so gtk-rs gir can parse them.
# Usage: fix-girs.py FILE.gir...  (edits in place)
import sys
import xml.etree.ElementTree as ET

CORE = "http://www.gtk.org/introspection/core/1.0"
NS = {
    "": CORE,
    "c": "http://www.gtk.org/introspection/c/1.0",
    "glib": "http://www.gtk.org/introspection/glib/1.0",
    "doc": "http://www.gtk.org/introspection/doc/1.0",
}
for prefix, uri in NS.items():
    ET.register_namespace(prefix, uri)

C_TYPE = "{%s}type" % NS["c"]
# Values g-ir-scanner gets wrong: macro arithmetic, string literals cut at
# ";" and a quoted string.
CONSTANTS = {
    "CAMEL_URL_PART_HIDDEN": "16",
    "E_SOURCE_PARAM_SETTING": "256",
    "E_NETWORK_MONITOR_ALWAYS_ONLINE_NAME": "always-online",
    "E_WEBDAV_COLLATION_ASCII_CASEMAP": "i;ascii-casemap",
    "E_WEBDAV_COLLATION_ASCII_NUMERIC": "i;ascii-numeric",
    "E_WEBDAV_COLLATION_OCTET": "i;octet",
    "E_WEBDAV_COLLATION_UNICODE_CASEMAP": "i;unicode-casemap",
}
DROPPABLE = {"function", "method", "constructor", "virtual-method",
             "callback", "property", "signal", "glib:signal"}


def tag(el):
    t = el.tag
    if t.startswith("{%s}" % CORE):
        return t[len(CORE) + 2:]
    if t.startswith("{%s}" % NS["glib"]):
        return "glib:" + t[len(NS["glib"]) + 2:]
    return t


def nameless(el):
    return any(t.get("name") is None
               for t in el.iter("{%s}type" % CORE))


def fix(path):
    tree = ET.parse(path)
    dropped = patched = 0
    parents = {c: p for p in tree.iter() for c in p}
    # Symbols from private headers are not installed.
    for el in list(tree.iter()):
        pos = el.find("{%s}source-position" % CORE)
        if (pos is not None and pos.get("filename", "").endswith("-private.h")
                and el in parents):
            parents[el].remove(el)
            dropped += 1
    for el in list(tree.iter()):
        if tag(el) not in DROPPABLE or el not in parents:
            continue
        if nameless(el):
            parents[el].remove(el)
            dropped += 1
    # Record fields keep their slot so struct layouts stay correct:
    # a nameless pointer type becomes gpointer.
    for field in tree.iter("{%s}field" % CORE):
        for t in field.iter("{%s}type" % CORE):
            if t.get("name") is None:
                t.set("name", "gpointer")
                t.set(C_TYPE, "gpointer")
                patched += 1

    root = tree.getroot()
    ns_el = root.find("{%s}namespace" % CORE)
    records = {}
    for kind in ("record", "class", "union"):
        for r in ns_el.findall("{%s}%s" % (CORE, kind)):
            if r.get(C_TYPE):
                records[r.get(C_TYPE)] = r.get("name")

    # Nested by-value structs in CamelCipherValidity have no typedef;
    # tests/manual.h in camel-sys supplies one for each.
    nested = {"_sign": "CipherValiditySignData",
              "_encrypt": "CipherValidityEncryptData"}
    for r in ns_el.findall("{%s}record" % CORE):
        if r.get("name") in nested:
            new = nested[r.get("name")]
            r.set("name", new)
            r.set(C_TYPE, "Camel" + new)
            records["Camel" + new] = new

    # gir skips type names with a leading underscore.
    names = {r.get("name") for r in ns_el}
    for r in list(ns_el):
        name = r.get("name") or ""
        if tag(r) in ("record", "union") and name.startswith("_"):
            new = name.lstrip("_")
            new = new[0].upper() + new[1:]
            if new in names:
                new = "Priv" + new
            names.add(new)
            r.set("name", new)
            for k, v in records.items():
                if v == name:
                    records[k] = new

    retyped = 0
    for t in tree.iter("{%s}type" % CORE):
        ctype = t.get(C_TYPE)
        if t.get("name") != "gpointer" or not ctype:
            continue
        base = ctype.rstrip("*").replace("const ", "").strip()
        stars = len(ctype) - len(ctype.rstrip("*"))
        if base in ("gpointer", "gconstpointer", "void"):
            continue
        if base in nested:
            new = nested[base]
            t.set("name", new)
            t.set(C_TYPE, "Camel" + new + "*" * stars)
        elif base in records:
            t.set("name", records[base])
        elif base.startswith("_") and base[1:] in records:
            t.set("name", records[base[1:]])
            t.set(C_TYPE, base[1:] + "*" * stars)
        elif base == "camel_filter_status_t":
            t.set("name", "gint")
            t.set(C_TYPE, "gint")
        elif stars:
            t.set(C_TYPE, "gpointer" + "*" * (stars - 1))
        else:
            raise SystemExit(f"{path}: unhandled by-value type {ctype}")
        retyped += 1

    # json-glib and libxml2 have no Rust -sys crates matching glib 0.22;
    # their pointers become gpointer.
    for t in tree.iter("{%s}type" % CORE):
        name = t.get("name") or ""
        ctype = t.get(C_TYPE) or ""
        if name.startswith(("Json.", "libxml2.")) and ctype.endswith("*"):
            stars = len(ctype) - len(ctype.rstrip("*"))
            t.set("name", "gpointer")
            t.set(C_TYPE, "gpointer" + "*" * (stars - 1))
            retyped += 1

    for t in tree.iter("{%s}type" % CORE):
        if t.get(C_TYPE) == "CamelVeeDataCache*" and t.get("name") == "VeeFolder":
            t.set("name", "VeeDataCache")
            retyped += 1

    # The camel-sys ABI tests check these against camel.h.
    for c in ns_el.findall("{%s}constant" % CORE):
        t = c.find("{%s}type" % CORE)
        if (t is not None and t.get("name") == "gint"
                and int(c.get("value", "0")) > 2**31 - 1):
            c.set("value", str(int(c.get("value")) - 2**32))
            retyped += 1
        if c.get(C_TYPE) in CONSTANTS:
            c.set("value", CONSTANTS[c.get(C_TYPE)])
    for r in ns_el.findall("{%s}record" % CORE):
        if r.get(C_TYPE) == "CamelProvider":
            for f in r.findall("{%s}field" % CORE):
                a = f.find("{%s}array" % CORE)
                if f.get("name") == "object_types" and a is not None:
                    a.set("fixed-size", "2")  # CAMEL_NUM_PROVIDER_TYPES
                    a.set(C_TYPE, "GType")
        # Array sizes depend on CAMEL_BLOCK_SIZE arithmetic the scanner
        # drops; these are only used through pointers, so they go opaque.
        if r.get(C_TYPE) in ("CamelPartitionKeyBlock",
                             "CamelPartitionMapBlock"):
            for f in r.findall("{%s}field" % CORE):
                r.remove(f)

    # Struct tags such as "_CamelFolderSummary*" become the typedef name;
    # gir drops vfuncs whose c:type differs from the record's.
    for t in tree.iter("{%s}type" % CORE):
        ctype = t.get(C_TYPE) or ""
        base = ctype.rstrip("*").replace("const ", "").strip()
        if base.startswith("_") and base[1:] in records:
            t.set(C_TYPE, ctype.replace(base, base[1:], 1))
            retyped += 1

    tree.write(path, encoding="UTF-8", xml_declaration=True)
    print(f"{path}: dropped {dropped}, patched {patched} fields, "
          f"retyped {retyped}")


for p in sys.argv[1:]:
    fix(p)
