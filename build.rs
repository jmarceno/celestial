use cxx_qt_build::{CxxQtBuilder, QmlFile, QmlModule};

fn main() {
    // Every QML file must be listed here AND live directly in qml/.
    let builder = CxxQtBuilder::new_qml_module(QmlModule::new("app.celestial").qml_files([
        QmlFile::from("qml/Main.qml"),
        QmlFile::from("qml/Shell.qml"),
        QmlFile::from("qml/CelestialTheme.qml"),
        QmlFile::from("qml/SidebarItem.qml"),
        QmlFile::from("qml/PrimaryButton.qml"),
        QmlFile::from("qml/Field.qml"),
        QmlFile::from("qml/TransferProgress.qml"),
        QmlFile::from("qml/RecentItem.qml"),
        QmlFile::from("qml/TrayPopup.qml"),
        QmlFile::from("qml/VScrollBar.qml"),
        QmlFile::from("qml/CloseButton.qml"),
        QmlFile::from("qml/TitleBar.qml"),
    ]))
    .files(["src/ui/controller.rs", "src/ui/branding.rs"])
    .qt_module("Quick")
    .qt_module("QuickControls2")
    .qt_module("Svg")
    .qrc_resources(["assets/com.unboundmark.Celestial-regular.svg"]);
    println!("cargo::rerun-if-changed=src/ui/branding.h");
    // SAFETY: only add the project root so the bridge can include our header.
    unsafe {
        builder.cc_builder(|cc| {
            cc.include(".");
        })
    }
    .build();
}
