// Stand-in for the webui library's client script: webui.call(name, ...args) becomes a POST to the dialog's own
// local server, resolving to the backend's reply string like the real webui does.
(function () {
    "use strict";
    async function call(name, ...args) {
        const response = await fetch("/call/" + encodeURIComponent(name), {
            method: "POST",
            headers: { "Content-Type": "application/json", "X-EveMaj-Token": window.__EVEMAJ_TOKEN__ || "" },
            body: JSON.stringify(args),
        });
        if (!response.ok) {
            throw new Error("webui.call(" + name + ") failed: HTTP " + response.status);
        }
        return await response.text();
    }
    window.webui = {
        call,
        isConnected: () => true,
        setLogging: () => {},
        setEventCallback: () => {},
    };
})();
