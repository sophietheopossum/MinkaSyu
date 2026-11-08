pragma ComponentBehavior: Bound
import QtQuick
import QtQuick.Layouts
import "../services"

// One browsable/installed package, as a ListView delegate: tick box,
// name, version + repo, description, and a marker (installed version,
// or "dependency"). Shared by the Browse and Installed pages -- the
// fields they feed it differ slightly, the row doesn't care.
RowLayout {
    id: root

    required property string name
    required property string version
    required property string repo
    property string description: ""
    property string marker: ""        // e.g. "installed 1.2-3" or "dependency"
    property bool selected: false
    signal toggled
    signal nameClicked

    spacing: 10

    CheckBox {
        checked: root.selected
        Layout.alignment: Qt.AlignTop
        Layout.topMargin: 2
        onToggled: root.toggled()
    }

    ColumnLayout {
        Layout.fillWidth: true
        spacing: 1
        opacity: root.selected ? 1.0 : 0.75

        RowLayout {
            spacing: 8
            Layout.fillWidth: true

            Text {
                text: root.name
                color: Theme.text
                font.family: Theme.fontFamily
                font.pixelSize: Theme.fontSize
                font.underline: nameArea.containsMouse
                elide: Text.ElideRight

                MouseArea {
                    id: nameArea
                    anchors.fill: parent
                    hoverEnabled: true
                    cursorShape: Qt.PointingHandCursor
                    onClicked: root.nameClicked()
                }
            }
            Text {
                text: root.version
                color: Theme.textMuted
                font.family: Theme.monoFamily
                font.pixelSize: Theme.fontSize - 2
                elide: Text.ElideRight
            }
            Text {
                text: root.repo
                color: root.repo === "aur" ? Theme.purple : Theme.textFaint
                font.family: Theme.monoFamily
                font.pixelSize: Theme.fontSize - 2
            }
            Item { Layout.fillWidth: true }
            Text {
                visible: root.marker.length > 0
                text: root.marker
                color: Theme.okGreen
                font.family: Theme.monoFamily
                font.pixelSize: Theme.fontSize - 2
            }
        }

        Text {
            visible: root.description.length > 0
            text: root.description
            color: Theme.textMuted
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 2
            wrapMode: Text.Wrap
            maximumLineCount: 2
            elide: Text.ElideRight
            Layout.fillWidth: true
        }
    }
}
