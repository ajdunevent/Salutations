//! The About dialog shows the release notes of the Cargo version from the
//! metainfo, and shows none when the metainfo lacks that release.

#[test]
fn metainfo_lists_cargo_version() {
    let xml = include_str!("../data/net.dunevent.Salutations.metainfo.xml");
    let release = format!("<release version=\"{}\"", env!("CARGO_PKG_VERSION"));
    assert!(xml.contains(&release), "add a {release}> entry to the metainfo");
}
