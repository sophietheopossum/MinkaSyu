pragma Singleton
import QtQuick
import Quickshell
// Through the config-root symlink: Quickshell only honours qmldir registration
// for paths inside the shell root.
import "../MinkaLink"

// The update core, spoken to over its stdio.
//
// Same shape as MinkaLedger's transport: NdjsonRpc owns the id
// correlation, CoreProcess owns the child, this object owns nothing but
// the pipe and the last state document handed to it. The core owns
// checking, repository provenance and the state file; this window is a
// view over `state`, refreshed on the core's {event:"state"} broadcast.
Singleton {
    id: root

    property string binary: Quickshell.env("MINKA_SYU_BIN") || "minka-syu"

    readonly property bool running: core.running
    property bool checking: false       // a check is in flight (seconds, not ms)
    property var state: null            // {schema, checked_at, updates, repo_change_count, ...}
    property string lastError: ""

    // Browsing. Search results belong to the last query only; the
    // installed list is a snapshot, re-asked on demand (the terminal
    // does the mutating, so nothing here can watch it change live).
    property bool searching: false
    property bool searched: false
    property var searchResults: []
    property bool searchTruncated: false
    property bool loadingInstalled: false
    property bool installedLoaded: false
    property var installedList: []

    // AppStream detail for the last-clicked package name. Loaded is
    // separate from the data: "loaded, and there was none" (null)
    // is a normal answer for libraries and most AUR packages.
    property string appstreamFor: ""
    property bool appstreamLoading: false
    property bool appstreamLoaded: false
    property var appstreamData: null

    // Requests made before the child process has finished starting. The
    // window asks for its state in Component.onCompleted, which fires
    // BEFORE Process.running flips true -- dropping those would leave a
    // permanently empty window that had merely asked too early.
    property var _pendingStart: []

    function request(method, params, onResult) {
        if (!core.running) {
            _pendingStart.push({ method, params, onResult });
            return;
        }
        rpc.request(method, params, (result, error) => {
            if (error)
                root.lastError = error.message || String(error.code);
            if (onResult)
                onResult(result, error);
        });
    }

    function _flushPendingStart() {
        const queued = _pendingStart;
        _pendingStart = [];
        for (const q of queued)
            root.request(q.method, q.params, q.onResult);
    }

    function refresh() {
        request("get_state", undefined, result => { root.state = result; });
    }

    function recheck() {
        if (checking)
            return;
        checking = true;
        lastError = "";
        // On success the core broadcasts the fresh state; the reply only
        // needs to end the in-flight flag. On failure there is no
        // broadcast and the old state stays on screen.
        request("check", undefined, () => { checking = false; });
    }

    // Opens the terminal running paru (the core owns the argv:
    // rio -e paru -Syu --sudoloop). `ignore` names updates unticked
    // in the list: skipped this once via --ignore, not remembered.
    // Nothing to show from the reply beyond a failed spawn, which
    // lands in lastError via request().
    function apply(ignore) {
        lastError = "";
        request("apply", { ignore: ignore ?? [] }, undefined);
    }

    function search(query) {
        if (searching || !query || query.trim().length === 0)
            return;
        searching = true;
        searched = true;
        lastError = "";
        request("search", { query: query.trim() }, (result, error) => {
            searching = false;
            if (!error && result) {
                root.searchResults = result.results ?? [];
                root.searchTruncated = result.truncated ?? false;
            }
        });
    }

    function loadInstalled() {
        if (loadingInstalled)
            return;
        loadingInstalled = true;
        lastError = "";
        request("installed", undefined, (result, error) => {
            loadingInstalled = false;
            if (!error && result) {
                root.installedList = result;
                root.installedLoaded = true;
            }
        });
    }

    function loadAppstream(name) {
        appstreamFor = name;
        appstreamData = null;
        appstreamLoaded = false;
        appstreamLoading = true;
        request("appstream", { package: name }, (result, error) => {
            // A slow answer for a package the user has clicked past
            // must not land in the now-open detail view.
            if (root.appstreamFor !== name)
                return;
            appstreamLoading = false;
            appstreamLoaded = true;
            if (!error)
                root.appstreamData = result;
        });
    }

    function installPackages(names) {
        if (!names || names.length === 0)
            return;
        lastError = "";
        request("install", { packages: names }, undefined);
    }

    function removePackages(names) {
        if (!names || names.length === 0)
            return;
        lastError = "";
        request("remove", { packages: names }, undefined);
    }

    NdjsonRpc {
        id: rpc
        writeLine: line => core.write(line)
        onBroadcast: (name, payload) => {
            if (name === "state") {
                root.state = payload;
                root.checking = false;
            }
        }
    }

    CoreProcess {
        id: core
        command: [root.binary, "serve"]

        onLine: line => rpc.feedLine(line)
        onStarted: {
            root.lastError = "";
            // Deferred, not synchronous: `started` is emitted from
            // inside the child's runningChanged notification, BEFORE
            // the `running` binding that request() checks has
            // re-evaluated. Flushing inline sees running=false and
            // re-queues everything into a queue nobody flushes again
            // -- the initial get_state never reached the core, and a
            // fresh window sat on "waiting for the core…" until
            // Recheck was clicked (verified headless, 7/11/2026).
            Qt.callLater(() => {
                root._flushPendingStart();
                root.refresh();
            });
        }
        onExited: {
            rpc.reset();
            root.checking = false;
            root.searching = false;
            root.loadingInstalled = false;
            root.appstreamLoading = false;
            root.lastError = "update core exited";
        }
    }
}
