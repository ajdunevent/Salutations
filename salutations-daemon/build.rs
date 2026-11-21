fn main() {
    for lib in ["libturbojpeg", "librsvg-2.0"] {
        pkg_config::Config::new().probe(lib).unwrap_or_else(|e| panic!("{lib}: {e}"));
    }
}
