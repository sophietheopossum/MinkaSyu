import QtQuick
import "../services"

// A button. Mirrors MinkaLedger's, with the primary action in the
// theme red: in this window the one consequential action upgrades the
// whole system, and purple is too calm for that.
Rectangle {
    id: root

    property string label
    property bool primary: false
    // A latched-on look for tab/toggle use, in purple: red is reserved
    // for the one consequential action per page.
    property bool toggled: false
    // `enabled` is inherited from Item: it already gates input and is what callers expect.

    signal clicked

    implicitWidth: caption.implicitWidth + 26
    implicitHeight: 30
    radius: 5
    opacity: root.enabled ? 1.0 : 0.4
    color: !root.enabled ? Theme.surface
         : root.primary ? (area.containsMouse ? Theme.red : Theme.redDim)
         : root.toggled ? Theme.purpleDim
                        : (area.containsMouse ? Theme.surfaceRaised : Theme.surface)
    border.width: 1
    border.color: root.primary ? Theme.red : root.toggled ? Theme.purple : Theme.line

    Text {
        id: caption
        anchors.centerIn: parent
        text: root.label
        font.family: Theme.fontFamily
        font.pixelSize: Theme.fontSize - 1
        color: Theme.text
    }

    MouseArea {
        id: area
        anchors.fill: parent
        hoverEnabled: true
        cursorShape: root.enabled ? Qt.PointingHandCursor : Qt.ArrowCursor
        onClicked: if (root.enabled) root.clicked()
    }
}
