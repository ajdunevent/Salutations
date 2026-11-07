fn main() {
    glib_build_tools::compile_resources(
        &["data"],
        "data/salutations.gresource.xml",
        "salutations.gresource",
    );
}
