use cxx_qt_build::{CxxQtBuilder, QmlFile, QmlModule};

fn main() {
    // Every QML file must be listed here AND live directly in qml/.
    CxxQtBuilder::new_qml_module(QmlModule::new("app.celestial").qml_files([
        QmlFile::from("qml/Main.qml"),
        QmlFile::from("qml/Shell.qml"),
        QmlFile::from("qml/CelestialTheme.qml"),
        QmlFile::from("qml/SidebarItem.qml"),
        QmlFile::from("qml/PrimaryButton.qml"),
        QmlFile::from("qml/Field.qml"),
    ]))
    .files(["src/ui/controller.rs"])
    .qt_module("Quick")
    .qt_module("QuickControls2")
    .qt_module("Svg")
    .qrc_resources(["assets/com.hunterwittenborn.Celestial-regular.svg"])
    .build();
}
