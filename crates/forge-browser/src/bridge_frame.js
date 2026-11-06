// The subframe half of the host opener bridge: refuse, never relay.
//
// **No native object and no relay are installed in a subframe.** A
// subframe's `window.open` is still wrapped, because a cross-site popup
// created there would be CEF's and would share this frame's renderer with
// it, the wedge the bridge exists to avoid.
//
// **A subframe never bridges.** A same-origin subframe can read the top's
// origin, so an http(s) address on that origin falls through to the engine's
// own popup path and anything else answers null; a cross-origin subframe
// cannot read the top's origin, so every http(s) address answers null. The
// other schemes are left to the engine either way.
(function () {
  'use strict';
  if (window === window.top) return;
  var savedOpen = window.open;
  var topOrigin = null;
  try {
    topOrigin = window.top.location.origin;
  } catch (e) {
    topOrigin = null;
  }

  function refused(url) {
    var parsed;
    try {
      parsed = new URL(String(url), location.href);
    } catch (e) {
      return false;
    }
    if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') return false;
    return topOrigin === null || parsed.origin !== topOrigin;
  }

  try {
    Object.defineProperty(window, 'open', {
      configurable: true,
      writable: true,
      value: function open(url, name, features) {
        if (refused(url)) return null;
        return savedOpen.apply(window, arguments);
      }
    });
  } catch (e) {}
})();
