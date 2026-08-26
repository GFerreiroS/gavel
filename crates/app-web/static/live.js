// Live cluster updates.
//
// One SSE connection replaces per-element polling: the server says *that*
// something changed, the page re-fetches only the fragments that care. The
// fragments keep a slow `every Nms` trigger as a fallback, so if the stream is
// unavailable the UI still converges -- just less promptly.
//
// Deliberately dependency-free: the htmx SSE extension would be another file to
// ship, and this is nine lines.
(function () {
  var RETRY_MS = 5000;
  var DEBOUNCE_MS = 200;
  var MAX_STALENESS_MS = 1000;

  function connect() {
    var source = new EventSource("/events/stream");

    // A 64-task job emits well over a hundred events in a second or two.
    // Without coalescing, each one would trigger a refetch of every live
    // fragment on every open tab -- the update storm would cost more than the
    // polling it replaced. One refresh per quiet 200ms, and at most one per
    // second under sustained load.
    var timer = null;
    var lastFired = 0;

    function scheduleRefresh() {
      var since = Date.now() - lastFired;
      if (since > MAX_STALENESS_MS) return fire();
      if (timer) clearTimeout(timer);
      timer = setTimeout(fire, DEBOUNCE_MS);
    }

    function fire() {
      if (timer) { clearTimeout(timer); timer = null; }
      lastFired = Date.now();
      document.body.dispatchEvent(new CustomEvent("cluster-changed"));
    }

    source.addEventListener("cluster", scheduleRefresh);

    source.onerror = function () {
      source.close();
      setTimeout(connect, RETRY_MS);
    };
  }

  connect();
})();
