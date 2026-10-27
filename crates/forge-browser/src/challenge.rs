//! A page's CAPTCHA frames and whether any of them is live.
//!
//! **Presence is not liveness.** Google's EXPIRED reCAPTCHA overlay is an
//! iframe parked at `top: -10000px` with `visibility: hidden`, and its rect is
//! still a real rect at viewport y = -13576, so presence and geometry both
//! read it as a live widget. The page answers the signals itself, where the
//! browser's own layout and hit testing are the truth.
//!
//! **A frame the probe cannot see is never reported absent.** The script reads
//! the top-level document, so every iframe it did not classify and every open
//! shadow root are named in `unseen`, which keeps an empty challenge list from
//! reading as "no CAPTCHA here".

use serde_json::{Value, json};

/// The vendors whose frames count as a challenge, matched case-insensitively
/// against an iframe's `src` and `title`.
///
/// **One list, and the page script is generated from it**: a vendor is added
/// here and nowhere else.
pub(crate) const VENDORS: &[(&str, &[&str])] = &[
    ("reCAPTCHA", &["recaptcha"]),
    ("hCaptcha", &["hcaptcha"]),
    ("Turnstile", &["turnstile", "challenges.cloudflare.com"]),
];

/// The page script, with [`VENDORS`] substituted in.
pub(crate) fn state_js() -> String {
    let vendors: Vec<Value> =
        VENDORS.iter().map(|(name, markers)| json!({ "name": name, "markers": markers })).collect();
    STATE_JS.replace("__VENDORS__", &Value::Array(vendors).to_string())
}

/// The script the page runs. One string, so what a test drives is what the
/// browser runs.
const STATE_JS: &str = r#"() => {
  const VENDORS = __VENDORS__;
  const viewport = { width: window.innerWidth, height: window.innerHeight };
  const name = (el) => {
    const id = el.id ? `#${el.id}` : "";
    const cls = typeof el.className === "string" && el.className.trim() ? `.${el.className.trim().split(/\s+/)[0]}` : "";
    return `${el.tagName.toLowerCase()}${id}${cls}`;
  };
  // Ancestor opacity multiplies and a filter's opacity(0) hides the subtree;
  // the frame's own computed opacity reports neither.
  const paintHidden = (el) => {
    let opacity = 1;
    for (let node = el; node && node.nodeType === 1; node = node.parentElement) {
      const style = getComputedStyle(node);
      opacity *= Number(style.opacity);
      if (opacity <= 0) return { hidden: true, opacity: 0 };
      const filter = style.filter || "none";
      if (filter !== "none" && /opacity\(\s*0*(?:\.0+)?%?\s*\)/.test(filter)) return { hidden: true, opacity: 0 };
    }
    return { hidden: false, opacity };
  };
  const vendorOf = (el) => {
    const named = `${el.getAttribute("src") || ""} ${el.getAttribute("title") || ""}`.toLowerCase();
    return VENDORS.find((entry) => entry.markers.some((marker) => named.includes(marker))) || null;
  };
  const challenges = [];
  const unseen = [];
  for (const frame of document.querySelectorAll("iframe")) {
    const vendor = vendorOf(frame);
    if (!vendor) {
      // Its document was not read, so a challenge inside it cannot be ruled out.
      unseen.push({ kind: "iframe", title: frame.getAttribute("title") || "", src: (frame.getAttribute("src") || "").slice(0, 300) });
      continue;
    }
    const rect = frame.getBoundingClientRect();
    const style = getComputedStyle(frame);
    const paint = paintHidden(frame);
    const visible = style.display !== "none" && style.visibility !== "hidden" && style.visibility !== "collapse" && !paint.hidden;
    const onScreen = rect.width > 0 && rect.height > 0 && rect.left < viewport.width && rect.top < viewport.height && rect.right > 0 && rect.bottom > 0;
    const hit = document.elementFromPoint(rect.left + rect.width / 2, rect.top + rect.height / 2);
    const hitTestable = hit === frame;
    challenges.push({
      present: true,
      vendor: vendor.name,
      title: frame.getAttribute("title") || "",
      src: (frame.getAttribute("src") || "").slice(0, 300),
      visible,
      onScreen,
      hitTestable,
      live: visible && onScreen && hitTestable,
      rect: { x: rect.left, y: rect.top, width: rect.width, height: rect.height },
      style: { display: style.display, visibility: style.visibility, opacity: style.opacity, filter: style.filter }
    });
  }
  for (const el of document.querySelectorAll("*")) {
    if (el.shadowRoot) unseen.push({ kind: "shadow-root", host: name(el) });
  }
  return { viewport, challengePresent: challenges.length > 0, complete: unseen.length === 0, challenges, unseen };
}"#;
