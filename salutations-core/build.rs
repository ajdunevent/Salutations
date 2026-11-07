//! Links libebook for contact completion (see src/contacts.rs).

fn main() {
    pkg_config::Config::new()
        .atleast_version("3.56")
        .probe("libebook-1.2")
        .expect("libebook-1.2 from pkg-config");
}
