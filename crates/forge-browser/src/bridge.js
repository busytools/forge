// The host opener bridge, installed in every frame before any page script.
//
// window.open is wrapped by the host: a cross-origin http(s) target answers a
// proxy the host relays, and every other target takes the engine's own popup
// path unchanged. A bridged popup's document gets window.opener as a matching
// proxy and window.close routed to the host, because a host-created tab is
// not a script-opened window as Blink counts them.
//
// The native object is the only IPC surface; this script is plain JS around
// it. Nothing here trusts the page: the host derives both sides' origins and
// ownership from the tab the message arrives on.
(function () {
  'use strict';
  var native = window.__forgeBridgeNative;
  if (!native) return;
  var W = window;
  var savedOpen = W.open;
  var channels = Object.create(null);
  var MARK = 'forge-opener-bridge';
  var popupMode = false;
  var openerState = { closed: false };
  var openerProxy = null;
  var openerOff = false;

  function securityError() {
    return new DOMException(
      'Blocked a frame with origin "' + location.origin +
        '" from accessing a cross-origin frame.', 'SecurityError');
  }

  function locationLike(navigate) {
    var loc = {};
    Object.defineProperty(loc, 'href', {
      get: function () { throw securityError(); },
      set: function (value) { navigate(String(value)); },
      enumerable: true
    });
    loc.assign = function (value) { navigate(String(value)); };
    loc.replace = function (value) { navigate(String(value)); };
    return loc;
  }

  function jsonOf(data) {
    if (data === undefined) return 'null';
    var text;
    try {
      text = JSON.stringify(data);
    } catch (e) {
      throw new DOMException('The data could not be cloned: ' + e.message, 'DataCloneError');
    }
    if (typeof text !== 'string') {
      // A function or a symbol has no JSON form and is not cloneable either.
      throw new DOMException('The data could not be cloned', 'DataCloneError');
    }
    return text;
  }

  // **A targetOrigin is `*` or an origin, nothing else.** A path-bearing URL
  // and an unparseable string both throw SyntaxError, as the platform's
  // postMessage does; the relay never reduces a URL to its origin for the
  // sender.
  function originOf(value) {
    if (typeof value !== 'string') {
      throw new TypeError('postMessage: targetOrigin must be a string');
    }
    if (value === '*') return value;
    var parsed;
    try {
      parsed = new URL(value);
    } catch (e) {
      throw new DOMException('postMessage: invalid targetOrigin', 'SyntaxError');
    }
    if (parsed.pathname !== '/' || parsed.search !== '' || parsed.hash !== '' ||
        parsed.username !== '' || parsed.password !== '') {
      throw new DOMException('postMessage: invalid targetOrigin', 'SyntaxError');
    }
    return value;
  }

  function handle() {
    var bytes = new Uint8Array(16);
    crypto.getRandomValues(bytes);
    var out = '';
    for (var i = 0; i < bytes.length; i += 1) {
      out += (bytes[i] < 16 ? '0' : '') + bytes[i].toString(16);
    }
    return out;
  }

  function proxyFor(token) {
    var state = { closed: false };
    var proxy = {};
    var location = locationLike(function (url) { native.navigate(token, url); });
    proxy.postMessage = function (data, targetOrigin) {
      native.post(token, originOf(targetOrigin), jsonOf(data));
    };
    Object.defineProperty(proxy, 'closed', {
      get: function () { return state.closed; },
      enumerable: true
    });
    proxy.close = function () { if (!state.closed) native.close(token); };
    proxy.focus = function () {};
    proxy.blur = function () {};
    Object.defineProperty(proxy, 'location', { get: function () { return location; } });
    Object.defineProperty(proxy, 'document', { get: function () { throw securityError(); } });
    proxy.window = proxy;
    proxy.self = proxy;
    proxy.top = proxy;
    proxy.parent = proxy;
    proxy.frames = proxy;
    return { proxy: proxy, state: state };
  }

  function openerFor() {
    var proxy = {};
    var location = locationLike(function (url) { native.navigate('', url); });
    proxy.postMessage = function (data, targetOrigin) {
      native.post('', originOf(targetOrigin), jsonOf(data));
    };
    Object.defineProperty(proxy, 'closed', {
      get: function () { return openerState.closed; },
      enumerable: true
    });
    proxy.close = function () { throw securityError(); };
    proxy.focus = function () {};
    proxy.blur = function () {};
    Object.defineProperty(proxy, 'location', { get: function () { return location; } });
    Object.defineProperty(proxy, 'document', { get: function () { throw securityError(); } });
    proxy.window = proxy;
    proxy.self = proxy;
    proxy.top = proxy;
    proxy.parent = proxy;
    proxy.frames = proxy;
    return proxy;
  }

  function enablePopup() {
    if (popupMode) return;
    popupMode = true;
    // **The role survives a navigation in `window.name`.** The browser's
    // `extra_info` reaches the first renderer only, and the host's assignment
    // is an IPC round trip that a document's inline script can beat. The
    // browsing context's name is the one store a later document can read
    // before its own scripts run, so it carries the marker.
    if (W.name === '' || W.name === MARK) W.name = MARK;
    openerProxy = openerFor();
    try {
      Object.defineProperty(W, 'opener', {
        configurable: true,
        get: function () { return openerOff ? null : openerProxy; },
        set: function (value) { if (value === null) openerOff = true; }
      });
    } catch (e) {}
    try {
      Object.defineProperty(W, 'close', {
        configurable: true,
        writable: true,
        value: function () { native.closeSelf(); }
      });
    } catch (e) {}
  }

  function crossSite(url) {
    var parsed;
    try { parsed = new URL(url, location.href); } catch (e) { return false; }
    if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') return false;
    return parsed.origin !== location.origin;
  }

  // **The bridge is the main frame's.** This script only runs in a main
  // frame; a subframe gets `bridge_frame.js`, which has no native object and
  // only refuses a cross-site open.
  function open(url, name, features) {
    var featuresText = typeof features === 'string' ? features.toLowerCase() : '';
    var noopener =
      /(^|[\s,])noopener($|[\s,])/.test(featuresText) ||
      /(^|[\s,])noreferrer($|[\s,])/.test(featuresText);
    if (!crossSite(String(url))) return savedOpen.apply(W, arguments);
    var token = handle();
    var resolved = String(new URL(String(url), location.href));
    var popupName = typeof name === 'string' ? name : '';
    if (noopener) {
      native.open(token, resolved, popupName, 1);
      return null;
    }
    var made = proxyFor(token);
    channels[token] = made;
    native.open(token, resolved, popupName, 0);
    return made.proxy;
  }

  // The host delivers here: one event per message, closed notice or refusal.
  W.__forgeBridgeDeliver = function (kind, token, origin, data, target) {
    if (kind === 'message') {
      // **The target origin is re-checked in the document that receives it.**
      // The host checks against the recipient's origin when it sends, but a
      // navigation can commit between that read and this dispatch; the
      // document itself is the only place the check cannot race.
      if (target !== '*') {
        var targetOrigin = null;
        try { targetOrigin = new URL(target).origin; } catch (e) { targetOrigin = null; }
        if (targetOrigin === null || targetOrigin !== location.origin) return;
      }
      var source = null;
      if (token && channels[token]) {
        source = channels[token].proxy;
      } else if (popupMode) {
        source = openerProxy;
      }
      var parsed = null;
      try { parsed = JSON.parse(data); } catch (e) { parsed = null; }
      var event = new MessageEvent('message', { data: parsed, origin: origin });
      try {
        Object.defineProperty(event, 'source', { value: source });
        Object.defineProperty(event, 'ports', { value: [] });
      } catch (e) {}
      W.dispatchEvent(event);
      return;
    }
    if (kind === 'closed') {
      if (token && channels[token]) {
        channels[token].state.closed = true;
      } else if (popupMode) {
        openerState.closed = true;
      }
      return;
    }
    if (kind === 'refused' && token && channels[token]) {
      channels[token].state.closed = true;
    }
  };

  W.__forgeBridgeEnablePopup = enablePopup;
  try {
    Object.defineProperty(W, 'open', {
      configurable: true,
      writable: true,
      value: open
    });
  } catch (e) {}
  if (W.__forgeBridgePopup === true || W.name === MARK) enablePopup();
})();
