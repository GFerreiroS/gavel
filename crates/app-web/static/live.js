// Live cluster updates.
//
// One SSE connection replaces per-element polling: the server says *that*
// something changed, the page re-fetches only the fragments that care. A slow
// poll takes over while the stream is down, so the UI still converges -- just
// less promptly -- and stops again the moment it comes back.
//
// Deliberately dependency-free: the htmx SSE extension would be another file to
// ship, and this is nine lines.
(function () {
  // htmx does not swap a 4xx or 5xx response by default, and every error this
  // app returns is already a rendered `.alert` fragment. The result was that a
  // rejected sign-in changed nothing on the page at all: no message, no hint,
  // just a form that appeared to ignore the button. Swapping the body is the
  // whole fix -- the status still marks it an error, so `htmx:responseError`
  // fires as before for anything listening.
  document.body.addEventListener("htmx:beforeSwap", function (evt) {
    if (evt.detail.xhr && evt.detail.xhr.status >= 400) {
      evt.detail.shouldSwap = true;
      evt.detail.isError = false;
    }
  });

  var RETRY_MS = 5000;
  var DEBOUNCE_MS = 200;
  var MAX_STALENESS_MS = 1000;

  // The fallback poll lives here rather than as an `every Nms` on every live
  // fragment, and that is the point: a trigger in the markup cannot be turned
  // off when the stream comes up, so the page was polling *and* listening --
  // the same work twice, and on a market page a refetch of every card.
  //
  // Now it is a fallback in the sense the word means: it runs only while the
  // stream is not open. `docs/market-analysis.md` §15: "Periodic HTMX polling
  // is a fallback only while SSE is disconnected; it does not run alongside a
  // healthy stream."
  var pollMs = parseInt(document.body.getAttribute("data-poll-ms"), 10);
  var poll = null;

  function startPolling() {
    if (poll || !pollMs) return;
    poll = setInterval(function () {
      document.body.dispatchEvent(new CustomEvent("cluster-changed"));
    }, pollMs);
  }

  function stopPolling() {
    if (!poll) return;
    clearInterval(poll);
    poll = null;
  }

  // Until the first connection opens there is no stream, so there is a poll.
  startPolling();

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

    source.onopen = stopPolling;

    source.onerror = function () {
      source.close();
      // Back to asking, until there is something listening again.
      startPolling();
      setTimeout(connect, RETRY_MS);
    };
  }

  connect();
})();
